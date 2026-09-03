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
    /// Set on every hop of a recurring chain: the id of the first hop's series, and the hop's
    /// position from 0. A chain is one commitment expressed as several series; see 0004.
    pub chain_id: Option<i64>,
    pub chain_seq: Option<i64>,
    pub postings: Vec<SeriesPosting>,
}

#[derive(Debug, Clone)]
pub struct Override {
    pub action: String, // amend | skip | add
    pub moved_to: Option<NaiveDate>,
    pub amount_minor: Option<Minor>,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InterestRule {
    pub id: i64,
    pub account_id: i64,
    pub counter_account_id: i64,
    pub shape: String,
    /// 'negative' = debt (cards, loans); 'positive' = savings.
    pub accrues_on: String,
    /// 'daily' compounds on the running balance; 'per_period' charges once at capitalisation.
    pub accrual_freq: String,
    pub capitalise_rrule: String,
    pub capitalise_dtstart: NaiveDate,
    pub periodic_rate_e15: i64,
    /// Cards with a grace period charge nothing while the balance is clear.
    pub grace_period: bool,
    pub scenario_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PaymentRule {
    pub id: i64,
    /// The account being paid down.
    pub account_id: i64,
    /// Where the money comes from.
    pub from_account_id: i64,
    pub amount_kind: String,
    pub fixed_minor: Option<Minor>,
    pub pct_e15: Option<i64>,
    pub floor_minor: Option<Minor>,
    pub cap_minor: Option<Minor>,
    pub level_payment_minor: Option<Minor>,
    pub rrule: String,
    pub dtstart: NaiveDate,
    pub until_on: Option<NaiveDate>,
    pub scenario_id: Option<i64>,
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
    pub interest_rules: Vec<InterestRule>,
    pub payment_rules: Vec<PaymentRule>,
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
    /// Which hop of a recurring chain this is, and how many there are; both absent for a plain
    /// series. Carried so the occurrence list and editor can say "leg 2 of 3".
    pub chain_seq: Option<i64>,
    pub chain_len: Option<i64>,
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

    // Cancelling any hop of a chain cancels the chain: the hops are one commitment, and a chain
    // with its first hop gone would show the intermediate account paying out of its own pocket.
    let suppressed_chains: HashSet<i64> = snap
        .series
        .iter()
        .filter(|s| suppressed.contains(&s.id))
        .filter_map(|s| s.chain_id)
        .collect();
    let mut chain_len: HashMap<i64, i64> = HashMap::new();
    for s in &snap.series {
        if let Some(c) = s.chain_id {
            *chain_len.entry(c).or_insert(0) += 1;
        }
    }

    let mut occurrences: Vec<Occurrence> = Vec::new();

    for s in &snap.series {
        match s.scenario_id {
            Some(id) if !active.contains(&id) => continue, // an inactive scenario's series
            _ => {}
        }
        if suppressed.contains(&s.id) || s.chain_id.is_some_and(|c| suppressed_chains.contains(&c)) {
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
                    chain_seq: s.chain_seq,
                    chain_len: s.chain_id.map(|c| chain_len[&c]),
                });
            }
        }
    }

    occurrences.sort_by(|a, b| {
        (&a.value_on, a.series_id, a.account_id).cmp(&(&b.value_on, b.series_id, b.account_id))
    });

    // ---- the day fold ----
    //
    // Interest cannot be expanded in advance like a series: its amount depends on the balance on
    // the day, and that balance is what we are computing. So the walk is genuinely sequential --
    // each day's interest changes every later day's balance.
    //
    // Order WITHIN a day: movements first, then accrual on the closing balance, then
    // capitalisation, then any payment. Accruing on the closing balance is the conventional
    // treatment and it means a payment landing today reduces today's interest, which is what a
    // cardholder expects when they pay early.
    let active_rule = |sid: &Option<i64>| match sid {
        Some(id) => active.contains(id),
        None => true,
    };
    let irules: Vec<&InterestRule> =
        snap.interest_rules.iter().filter(|r| active_rule(&r.scenario_id)).collect();
    let prules: Vec<&PaymentRule> =
        snap.payment_rules.iter().filter(|r| active_rule(&r.scenario_id)).collect();

    // Pre-expand the DATES interest events land on. Only the dates -- the amounts are unknowable
    // until the walk reaches them.
    let mut capitalise_on: HashMap<i64, HashSet<NaiveDate>> = HashMap::new();
    for r in &irules {
        let days = recur.expand(&r.capitalise_rrule, r.capitalise_dtstart, None, as_of, horizon)?;
        capitalise_on.insert(r.id, days.into_iter().collect());
    }
    let mut pay_on: HashMap<i64, HashSet<NaiveDate>> = HashMap::new();
    for r in &prules {
        let days = recur.expand(&r.rrule, r.dtstart, r.until_on, as_of, horizon)?;
        pay_on.insert(r.id, days.into_iter().collect());
    }

    let mut running: BTreeMap<i64, Minor> = snap.opening.clone();
    let mut balances: Vec<BalancePoint> = Vec::new();
    // Interest accrued but not yet posted, per rule.
    let mut accrued: HashMap<i64, Minor> = HashMap::new();
    // The balance frozen at the last capitalisation -- what a minimum payment is a percentage OF.
    let mut statement: HashMap<i64, Minor> = HashMap::new();
    let mut by_day: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, o) in occurrences.iter().enumerate() {
        by_day.entry(o.value_on.clone()).or_default().push(idx);
    }

    let mut extra: Vec<Occurrence> = Vec::new();
    let mut day = as_of;
    let need_walk = !irules.is_empty() || !prules.is_empty();
    while day <= horizon {
        let key = day.to_string();
        let mut touched: Vec<i64> = Vec::new();

        if let Some(idxs) = by_day.get(&key) {
            for &idx in idxs {
                let o = &occurrences[idx];
                *running.entry(o.account_id).or_insert(0) += o.amount_minor;
                if !touched.contains(&o.account_id) {
                    touched.push(o.account_id);
                }
            }
        }

        if need_walk {
            for r in &irules {
                let bal = *running.get(&r.account_id).unwrap_or(&0);
                // A rule only bites on the side of zero it is written for: a card charges on debt,
                // a savings account pays on credit. Wrong-side balances accrue nothing.
                let engaged = match r.accrues_on.as_str() {
                    "negative" => bal < 0,
                    _ => bal > 0,
                };
                // Grace: a card that owes nothing charges nothing.
                let graced = r.grace_period && !engaged;
                if r.accrual_freq == "daily" && engaged && !graced {
                    let today = crate::interest::accrue(bal, r.periodic_rate_e15).unwrap_or(0);
                    *accrued.entry(r.id).or_insert(0) += today;
                }

                if capitalise_on.get(&r.id).is_some_and(|d| d.contains(&day)) {
                    // per_period charges once, on the balance standing at capitalisation.
                    if r.accrual_freq == "per_period" && engaged && !graced {
                        let amount =
                            crate::interest::accrue(bal, r.periodic_rate_e15).unwrap_or(0);
                        *accrued.entry(r.id).or_insert(0) += amount;
                    }
                    let amount = accrued.remove(&r.id).unwrap_or(0);
                    if amount != 0 {
                        // Interest posts in the account's OWN currency; this engine never converts.
                        let cur = snap.account_currency.get(&r.account_id).cloned().unwrap_or_default();
                        for (acct, amt) in
                            [(r.account_id, amount), (r.counter_account_id, -amount)]
                        {
                            *running.entry(acct).or_insert(0) += amt;
                            if !touched.contains(&acct) {
                                touched.push(acct);
                            }
                            extra.push(Occurrence {
                                series_id: -r.id, // negative marks a generated interest leg
                                occurrence_on: key.clone(),
                                value_on: key.clone(),
                                description: format!("Interest ({})", r.shape),
                                account_id: acct,
                                account: snap.account_name.get(&acct).cloned().unwrap_or_default(),
                                currency: cur.clone(),
                                amount_minor: amt,
                            chain_seq: None,
                            chain_len: None,
                            });
                        }
                    }
                    // Keyed by ACCOUNT, not by rule id: the statement balance is a property of
                    // the card, and the interest rule and the payment rule that reads it are in
                    // different id spaces entirely.
                    statement.insert(r.account_id, *running.get(&r.account_id).unwrap_or(&0));
                }
            }

            for r in &prules {
                if !pay_on.get(&r.id).is_some_and(|d| d.contains(&day)) {
                    continue;
                }
                let bal = *running.get(&r.account_id).unwrap_or(&0);
                // Debt is held negative; payment sizing works in positive magnitudes.
                let owed = (-bal).max(0);
                let stmt = statement
                    .get(&r.account_id)
                    .map(|s| (-s).max(0))
                    .unwrap_or(owed);
                let ctx = crate::interest::PaymentContext {
                    balance_minor: owed,
                    statement_minor: stmt,
                    interest_minor: 0,
                    fees_minor: 0,
                };
                let amount = crate::interest::payment_amount(
                    &r.amount_kind, ctx, r.fixed_minor, r.pct_e15, r.floor_minor, r.cap_minor,
                    r.level_payment_minor,
                )
                .unwrap_or(0);
                if amount != 0 {
                    let cur =
                        snap.account_currency.get(&r.account_id).cloned().unwrap_or_default();
                    for (acct, amt) in
                        [(r.account_id, amount), (r.from_account_id, -amount)]
                    {
                        *running.entry(acct).or_insert(0) += amt;
                        if !touched.contains(&acct) {
                            touched.push(acct);
                        }
                        extra.push(Occurrence {
                            series_id: -r.id,
                            occurrence_on: key.clone(),
                            value_on: key.clone(),
                            description: "Payment".to_string(),
                            account_id: acct,
                            account: snap.account_name.get(&acct).cloned().unwrap_or_default(),
                            currency: cur.clone(),
                            amount_minor: amt,
                            chain_seq: None,
                            chain_len: None,
                        });
                    }
                }
            }
        }

        if !touched.is_empty() {
            touched.sort_unstable();
            for account_id in touched {
                balances.push(BalancePoint {
                    account_id,
                    account: snap.account_name.get(&account_id).cloned().unwrap_or_default(),
                    currency: snap.account_currency.get(&account_id).cloned().unwrap_or_default(),
                    on: key.clone(),
                    balance_minor: running[&account_id],
                });
            }
        }
        day = match day.succ_opt() {
            Some(d) => d,
            None => break,
        };
    }
    occurrences.extend(extra);
    occurrences.sort_by(|a, b| {
        (&a.value_on, a.series_id, a.account_id).cmp(&(&b.value_on, b.series_id, b.account_id))
    });

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
            chain_id: None,
            chain_seq: None,
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
            chain_id: None,
            chain_seq: None,
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
            chain_id: None,
            chain_seq: None,
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
            chain_id: None,
            chain_seq: None,
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

    /// A credit card: a liability account with an interest rule and a minimum-payment rule.
    fn card_book(pay_kind: &str, pay_amount: Option<Minor>, pct: Option<i64>) -> Snapshot {
        let mut s = Snapshot::default();
        for (id, name) in [(1, "Current"), (5, "Card"), (6, "Interest paid")] {
            s.account_name.insert(id, name.into());
            s.account_currency.insert(id, "GBP".into());
        }
        s.opening.insert(1, 500_000);  // £5,000 in the current account
        s.opening.insert(5, -200_000); // £2,000 owed on the card
        // 24.9% AER, accrued daily, capitalised on the 1st.
        let rate = crate::interest::derive_periodic_rate(249_000_000_000_000, "effective", 365).unwrap();
        s.interest_rules.push(InterestRule {
            id: 1, account_id: 5, counter_account_id: 6,
            shape: "revolving".into(), accrues_on: "negative".into(), accrual_freq: "daily".into(),
            capitalise_rrule: "FREQ=MONTHLY;BYMONTHDAY=1".into(),
            capitalise_dtstart: d("2026-01-01"), periodic_rate_e15: rate,
            grace_period: true, scenario_id: None,
        });
        s.payment_rules.push(PaymentRule {
            id: 1, account_id: 5, from_account_id: 1,
            amount_kind: pay_kind.into(), fixed_minor: pay_amount, pct_e15: pct,
            floor_minor: Some(500), cap_minor: None, level_payment_minor: None,
            rrule: "FREQ=MONTHLY;BYMONTHDAY=15".into(), dtstart: d("2026-01-15"),
            until_on: None, scenario_id: None,
        });
        s
    }

    #[test]
    fn a_card_paying_only_the_minimum_barely_moves() {
        // 1% of the statement, floored at £5 -- the classic trap. Over a year the balance should
        // fall only slightly, because most of each payment is interest.
        let s = card_book("pct_of_statement", None, Some(10_000_000_000_000));
        let p = run(&s, "2026-01-01", "2026-12-31", &[]);
        let end = closing(&p, 5);
        assert!(end < 0, "still in debt after a year of minimums: {end}");
        assert!(end > -200_000 - 60_000 && end < -150_000,
                "minimum payments barely dent £2,000 at 24.9%: got {end}");
        // and interest legs were actually generated
        assert!(p.occurrences.iter().any(|o| o.description.starts_with("Interest")));
    }

    #[test]
    fn paying_more_clears_the_card_and_composes_as_a_scenario() {
        // The requirement-9 question: "what if I pay £250 a month instead?"
        let mut s = card_book("pct_of_statement", None, Some(10_000_000_000_000));
        s.payment_rules.push(PaymentRule {
            id: 2, account_id: 5, from_account_id: 1,
            amount_kind: "fixed".into(), fixed_minor: Some(25_000), pct_e15: None,
            floor_minor: None, cap_minor: None, level_payment_minor: None,
            rrule: "FREQ=MONTHLY;BYMONTHDAY=15".into(), dtstart: d("2026-01-15"),
            until_on: None, scenario_id: Some(1),
        });
        let minimum = run(&s, "2026-01-01", "2026-12-31", &[]);
        let harder = run(&s, "2026-01-01", "2026-12-31", &[1]);
        assert!(closing(&harder, 5) > closing(&minimum, 5),
                "paying more must leave less debt");
        // Cleared, and very slightly IN CREDIT. That is not a rounding artefact: a payment sized
        // from the statement pays what the statement said, and by the time it lands the balance has
        // fallen below that. A standing order overshooting a nearly-cleared card is exactly what
        // happens in life, so the projection shows it rather than clamping it away.
        let end = closing(&harder, 5);
        assert!((0..1_000).contains(&end), "cleared, possibly a few pounds in credit: got {end}");
        assert!(closing(&minimum, 5) < -150_000, "the minimum-only path is still deep in debt");
    }

    #[test]
    fn a_cleared_card_with_a_grace_period_charges_nothing() {
        let mut s = card_book("fixed", Some(0), None);
        s.opening.insert(5, 0); // nothing owed
        let p = run(&s, "2026-01-01", "2026-06-30", &[]);
        assert!(!p.occurrences.iter().any(|o| o.description.starts_with("Interest")),
                "a clear card in its grace period accrues nothing");
    }

    #[test]
    fn an_amortising_loan_runs_down_to_zero() {
        let mut s = Snapshot::default();
        for (id, name) in [(1, "Current"), (7, "Loan"), (8, "Loan interest")] {
            s.account_name.insert(id, name.into());
            s.account_currency.insert(id, "GBP".into());
        }
        s.opening.insert(1, 5_000_000);
        s.opening.insert(7, -1_000_000); // £10,000 borrowed
        let rate = crate::interest::derive_periodic_rate(60_000_000_000_000, "nominal", 12).unwrap();
        let level = crate::interest::derive_level_payment(1_000_000, rate, 60).unwrap();
        s.interest_rules.push(InterestRule {
            id: 2, account_id: 7, counter_account_id: 8,
            shape: "amortising".into(), accrues_on: "negative".into(),
            accrual_freq: "per_period".into(),
            capitalise_rrule: "FREQ=MONTHLY;BYMONTHDAY=1".into(),
            capitalise_dtstart: d("2026-01-01"), periodic_rate_e15: rate,
            grace_period: false, scenario_id: None,
        });
        s.payment_rules.push(PaymentRule {
            id: 3, account_id: 7, from_account_id: 1,
            amount_kind: "amortising_level".into(), fixed_minor: None, pct_e15: None,
            floor_minor: None, cap_minor: None, level_payment_minor: Some(level),
            rrule: "FREQ=MONTHLY;BYMONTHDAY=2".into(), dtstart: d("2026-01-02"),
            until_on: None, scenario_id: None,
        });
        // 60 monthly payments from Jan 2026
        let p = run(&s, "2026-01-01", "2030-12-31", &[]);
        let end = closing(&p, 7);
        assert_eq!(end, 0, "the loan must run down to exactly zero, got {end}");
    }

    #[test]
    fn savings_interest_compounds_in_your_favour() {
        let mut s = Snapshot::default();
        for (id, name) in [(9, "Savings"), (10, "Interest earned")] {
            s.account_name.insert(id, name.into());
            s.account_currency.insert(id, "GBP".into());
        }
        s.opening.insert(9, 500_000); // £5,000
        let rate = crate::interest::derive_periodic_rate(45_000_000_000_000, "effective", 12).unwrap();
        s.interest_rules.push(InterestRule {
            id: 3, account_id: 9, counter_account_id: 10,
            shape: "savings".into(), accrues_on: "positive".into(),
            accrual_freq: "per_period".into(),
            capitalise_rrule: "FREQ=MONTHLY;BYMONTHDAY=1".into(),
            capitalise_dtstart: d("2026-01-01"), periodic_rate_e15: rate,
            grace_period: false, scenario_id: None,
        });
        let p = run(&s, "2026-01-01", "2026-12-31", &[]);
        let end = closing(&p, 9);
        // 12 monthly credits at 4.5% AER on £5,000 -- about £225 earned
        assert!((522_000..=523_000).contains(&end), "a year at 4.5% AER: got {end}");
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
                    supersedes_id, chain_id, chain_seq FROM series",
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
                    chain_id: r.get(8)?,
                    chain_seq: r.get(9)?,
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

        // Interest rules, each with the rate period in force at `as_of`. A rule with no rate
        // period is skipped rather than defaulted: a silent 0% would read as "no interest" and be
        // indistinguishable from a correct projection.
        let mut st = conn.prepare(
            "SELECT r.id, r.account_id, r.counter_account_id, r.shape, r.accrues_on,
                    r.accrual_freq, r.capitalise_rrule, r.capitalise_dtstart, r.grace_period,
                    r.scenario_id,
                    (SELECT p.periodic_rate_e15 FROM interest_rate_period p
                      WHERE p.rule_id = r.id AND p.effective_from <= ?1
                        AND (p.effective_to IS NULL OR p.effective_to > ?1)
                      ORDER BY p.effective_from DESC LIMIT 1)
               FROM interest_rule r",
        )?;
        for row in st.query_map([as_of.to_string()], |r| {
            Ok((
                InterestRule {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    counter_account_id: r.get(2)?,
                    shape: r.get(3)?,
                    accrues_on: r.get(4)?,
                    accrual_freq: r.get(5)?,
                    capitalise_rrule: r.get(6)?,
                    capitalise_dtstart: date(r.get(7)?),
                    periodic_rate_e15: 0,
                    grace_period: r.get::<_, i64>(8)? == 1,
                    scenario_id: r.get(9)?,
                },
                r.get::<_, Option<i64>>(10)?,
            ))
        })? {
            let (mut rule, rate) = row?;
            if let Some(rate) = rate {
                rule.periodic_rate_e15 = rate;
                snap.interest_rules.push(rule);
            }
        }

        let mut st = conn.prepare(
            "SELECT id, account_id, from_account_id, amount_kind, fixed_minor, pct_e15,
                    floor_minor, cap_minor, level_payment_minor, rrule, dtstart, until_on,
                    scenario_id FROM payment_rule",
        )?;
        for row in st.query_map([], |r| {
            Ok(PaymentRule {
                id: r.get(0)?,
                account_id: r.get(1)?,
                from_account_id: r.get(2)?,
                amount_kind: r.get(3)?,
                fixed_minor: r.get(4)?,
                pct_e15: r.get(5)?,
                floor_minor: r.get(6)?,
                cap_minor: r.get(7)?,
                level_payment_minor: r.get(8)?,
                rrule: r.get(9)?,
                dtstart: date(r.get(10)?),
                until_on: r.get::<_, Option<String>>(11)?.map(date),
                scenario_id: r.get(12)?,
            })
        })? {
            snap.payment_rules.push(row?);
        }

        let mut st = conn.prepare("SELECT on_date FROM holiday")?;
        for row in st.query_map([], |r| r.get::<_, String>(0))? {
            snap.holidays.push(date(row?));
        }

        Ok(snap)
    }
}
