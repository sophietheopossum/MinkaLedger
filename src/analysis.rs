//! Analysis: the shape a language model can actually reason about.
//!
//! `export` makes the book AVAILABLE, which is not the same as making analysis EFFECTIVE, and the
//! gap between those two is this whole module. Handed four thousand ledger lines, a model will:
//!
//!   - get the arithmetic wrong, quietly, because summing hundreds of integers is not what it does;
//!   - not know which outgoings are committed and which are choices;
//!   - have no baseline, so "you spent 340 on groceries" arrives without "you usually spend 280";
//!   - annualise a 4-weekly payment as x12 and land 8% under;
//!   - and, on a real book, exhaust its context and analyse a truncated prefix without saying so.
//!
//! None of those are fixed by sending more data. They are fixed by sending fewer numbers, already
//! computed. So every figure here is derived in integer arithmetic by the same engine that draws
//! the forecast -- never a second implementation -- and the document says how each was derived,
//! because a model that cannot see a derivation will helpfully redo it.
//!
//! `limits` is the part not to delete. It states what the book CANNOT tell you: how much spending
//! is unclassified, how far back the data reaches, how many complete months a median rests on. An
//! analysis blind to its own blind spots is worse than none, because it is confident.

use crate::forecast;
use crate::money::{self, Minor};
use crate::recur::RRuleCrate;
use chrono::{Datelike, NaiveDate};
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug)]
pub enum AnalysisError {
    Sql(rusqlite::Error),
    Bad(String),
}

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalysisError::Sql(e) => write!(f, "{e}"),
            AnalysisError::Bad(m) => write!(f, "{m}"),
        }
    }
}

impl From<rusqlite::Error> for AnalysisError {
    fn from(e: rusqlite::Error) -> Self {
        AnalysisError::Sql(e)
    }
}

/// What a reader must know about the BRIEF specifically. The export preamble covers reading raw
/// postings; these cover the traps in reading numbers somebody else already worked out.
pub const BRIEF_PREAMBLE: &[&str] = &[
    "Every figure here is already computed from the full book in exact integer arithmetic. Use these numbers. Do not re-derive them by adding up lines you were shown -- you were not shown all the lines.",
    "monthly_equivalent_minor is NOT the amount times twelve-over-period. It is the next twelve months of the actual RFC 5545 recurrence, summed and divided by twelve, so a 4-weekly payment correctly costs 13/12 of a monthly one and a rule with an end date correctly costs less.",
    "months[] holds COMPLETE calendar months only. The month in progress is deliberately excluded: comparing a part month against whole ones is the most common way to invent a trend that is not there.",
    "spent_minor and received_minor are POSITIVE magnitudes, converted from the ledger's own signs for readability. by_account[].amount_minor keeps the ledger convention (expenses positive, income negative). Do not mix the two.",
    "commitments are what RECURS. They are not all of your spending, and an outgoing that is absent means 'not set up as a recurring series', never 'does not happen'.",
    "outlook is a PROJECTION. It has not happened, it assumes today's rules continue unchanged, and it must never be added into a total of what was actually spent.",
    "Every amount is an integer in its currency's minor unit, and each block is grouped by currency. Two currencies are never comparable and must never be summed. The divisor is 10^minor_digits, which is not always 100.",
    "Read `limits` before concluding anything. It states what this book cannot tell you, and several of its entries invalidate whole sections when they fire.",
];

// ---------------------------------------------------------------------------------------------
// small date helpers -- chrono has no month arithmetic that answers "the month before this one"
// ---------------------------------------------------------------------------------------------

fn month_start(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

/// Shift by whole months, clamping the day. Only ever called with day 1, so the clamp is belt.
fn shift_months(d: NaiveDate, delta: i32) -> NaiveDate {
    let total = d.year() * 12 + (d.month() as i32 - 1) + delta;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    NaiveDate::from_ymd_opt(y, m, d.day())
        .or_else(|| NaiveDate::from_ymd_opt(y, m, 1))
        .unwrap_or(d)
}

/// Median of already-sorted values, halves away from zero -- the book's single rounding rule.
fn median(sorted: &[Minor]) -> Minor {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        let a = sorted[n / 2 - 1] as i128;
        let b = sorted[n / 2] as i128;
        money::round_half_away(a + b, 2).unwrap_or(sorted[n / 2])
    }
}

fn minor_digits(conn: &Connection) -> Result<HashMap<String, u32>, AnalysisError> {
    let mut st = conn.prepare("SELECT code, minor_digits FROM currency")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32)))?;
    Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
}

/// Format helper bound to the book's currency scales, so no caller ever assumes 2dp.
struct Scales(HashMap<String, u32>);
impl Scales {
    fn dec(&self, amount: Minor, currency: &str) -> String {
        money::format_minor(amount, *self.0.get(currency).unwrap_or(&2))
    }
}

// ---------------------------------------------------------------------------------------------
// sections
// ---------------------------------------------------------------------------------------------

/// Where the money is right now: per account, and netted per currency.
///
/// "Right now" is as_of, the same day the outlook starts from and the account list shows: a
/// payment typed in for next week is in the outlook's dip on that day, not in today's position.
///
/// Netting stops at the currency boundary on purpose. A "total net worth" across GBP and JPY is a
/// number with no meaning, and offering one is an invitation to quote it.
fn position(conn: &Connection, sc: &Scales, as_of: NaiveDate) -> Result<serde_json::Value, AnalysisError> {
    let mut st = conn.prepare(
        "SELECT a.name, a.kind, p.currency, SUM(p.amount_minor), MAX(t.occurred_on)
           FROM posting p
           JOIN txn t     ON t.id = p.txn_id
           JOIN account a ON a.id = p.account_id
          WHERE a.kind IN ('asset','liability') AND a.system = 0
            AND t.occurred_on <= ?1
          GROUP BY a.id, p.currency
          ORDER BY a.kind, a.name",
    )?;
    let accounts: Vec<serde_json::Value> = st
        .query_map([as_of.to_string()], |r| {
            let (name, kind, cur, bal, last) = (
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Minor>(3)?,
                r.get::<_, Option<String>>(4)?,
            );
            Ok(serde_json::json!({
                "account": name, "kind": kind, "currency": cur,
                "balance_minor": bal, "balance_decimal": sc.dec(bal, &cur),
                "last_movement_on": last,
            }))
        })?
        .collect::<Result<_, _>>()?;

    let mut net: BTreeMap<String, Minor> = BTreeMap::new();
    for a in &accounts {
        let cur = a["currency"].as_str().unwrap_or_default().to_string();
        *net.entry(cur).or_insert(0) += a["balance_minor"].as_i64().unwrap_or(0);
    }

    Ok(serde_json::json!({
        "derivation": "sum of every posting dated on or before as_of on non-system asset and liability accounts",
        "by_account": accounts,
        "net_by_currency": net.iter().map(|(c, v)| serde_json::json!({
            "currency": c, "amount_minor": v, "amount_decimal": sc.dec(*v, c),
        })).collect::<Vec<_>>(),
    }))
}

/// What recurs, and what it really costs per month.
///
/// The monthly figure is the reason this is not a table of `series_posting.amount_minor`. Twelve
/// months are genuinely expanded through the projection engine, so end dates, skipped occurrences,
/// amended amounts and weekend adjustment are all already in the number -- and 4-weekly does not
/// get mistaken for monthly, which is the error that makes a budget quietly 8% optimistic.
fn commitments(
    conn: &Connection,
    sc: &Scales,
    as_of: NaiveDate,
) -> Result<serde_json::Value, AnalysisError> {
    let snap = forecast::load::snapshot(conn, as_of)?;
    let year_end = shift_months(as_of, 12);
    let proj = forecast::project(&RRuleCrate, &snap, as_of, year_end, &HashSet::new())
        .map_err(|e| AnalysisError::Bad(e.to_string()))?;

    // Only the PRIMARY leg counts: the balancing leg is the same money seen from the other side,
    // and summing both would net every commitment to exactly zero.
    let mut primary: HashMap<i64, i64> = HashMap::new();
    for s in &snap.series {
        // A recurring chain is one commitment spread over several series; the money leaves the
        // household once, on the first hop. Counting every hop would report a £100 stake routed
        // through a friend as £200 of outgoings.
        if s.chain_seq.is_some_and(|q| q > 0) {
            continue;
        }
        if let Some(p) = s.postings.iter().find(|p| p.role == "primary") {
            primary.insert(s.id, p.account_id);
        }
    }

    struct Agg {
        annual: i128,
        count: i64,
        currency: String,
        account: String,
        next_on: Option<String>,
    }
    let mut by_series: BTreeMap<i64, Agg> = BTreeMap::new();
    for o in &proj.occurrences {
        if primary.get(&o.series_id) != Some(&o.account_id) {
            continue;
        }
        let e = by_series.entry(o.series_id).or_insert_with(|| Agg {
            annual: 0,
            count: 0,
            currency: o.currency.clone(),
            account: o.account.clone(),
            next_on: None,
        });
        e.annual += o.amount_minor as i128;
        e.count += 1;
        if e.next_on.is_none() {
            e.next_on = Some(o.value_on.clone());
        }
    }

    let described: HashMap<i64, (String, String)> = snap
        .series
        .iter()
        .map(|s| (s.id, (s.description.clone(), s.rrule.clone())))
        .collect();

    let mut rows = Vec::new();
    let mut monthly_by_currency: BTreeMap<String, i128> = BTreeMap::new();
    for (sid, a) in &by_series {
        let monthly = money::round_half_away(a.annual, 12).unwrap_or(0);
        *monthly_by_currency.entry(a.currency.clone()).or_insert(0) += monthly as i128;
        let (desc, rrule) = described.get(sid).cloned().unwrap_or_default();
        let annual = i64::try_from(a.annual).unwrap_or(i64::MAX);
        rows.push(serde_json::json!({
            "series_id": sid,
            "description": desc,
            "rrule": rrule,
            "account": a.account,
            "currency": a.currency,
            "next_on": a.next_on,
            "occurrences_next_12m": a.count,
            "annual_minor": annual,
            "annual_decimal": sc.dec(annual, &a.currency),
            "monthly_equivalent_minor": monthly,
            "monthly_equivalent_decimal": sc.dec(monthly, &a.currency),
        }));
    }
    // Largest commitment first: the reader almost always wants the top of this list.
    rows.sort_by_key(|r| r["monthly_equivalent_minor"].as_i64().unwrap_or(0));

    Ok(serde_json::json!({
        "derivation": format!(
            "every non-scenario series expanded from {as_of} to {year_end} through the projection \
             engine, primary leg only, a chain counted once by its first hop, summed and divided by 12"),
        "window": { "from": as_of.to_string(), "to": year_end.to_string() },
        "series": rows,
        "monthly_equivalent_by_currency": monthly_by_currency.iter().map(|(c, v)| {
            let v = i64::try_from(*v).unwrap_or(i64::MAX);
            serde_json::json!({ "currency": c, "amount_minor": v, "amount_decimal": sc.dec(v, c) })
        }).collect::<Vec<_>>(),
        "note": "A negative monthly_equivalent is money leaving; income series are positive. \
                 Sorted most-costly first.",
    }))
}

struct MonthCell {
    account: String,
    kind: String,
    currency: String,
    amount: Minor,
}

/// Complete calendar months only, per account, plus the spent/received magnitudes.
fn monthly(
    conn: &Connection,
    sc: &Scales,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(Vec<serde_json::Value>, BTreeMap<String, Vec<MonthCell>>), AnalysisError> {
    let mut st = conn.prepare(
        "SELECT strftime('%Y-%m', t.occurred_on) AS ym, a.name, a.kind, p.currency,
                SUM(p.amount_minor)
           FROM posting p
           JOIN txn t     ON t.id = p.txn_id
           JOIN account a ON a.id = p.account_id
          WHERE a.kind IN ('income','expense')
            AND t.occurred_on >= ?1 AND t.occurred_on <= ?2
          GROUP BY ym, a.id, p.currency
          ORDER BY ym, a.name",
    )?;
    let mut buckets: BTreeMap<String, Vec<MonthCell>> = BTreeMap::new();
    let rows = st.query_map(rusqlite::params![from.to_string(), to.to_string()], |r| {
        Ok((
            r.get::<_, String>(0)?,
            MonthCell {
                account: r.get(1)?,
                kind: r.get(2)?,
                currency: r.get(3)?,
                amount: r.get(4)?,
            },
        ))
    })?;
    for row in rows {
        let (ym, cell) = row?;
        buckets.entry(ym).or_default().push(cell);
    }

    let mut months = Vec::new();
    for (ym, cells) in &buckets {
        // Per currency within the month, because a month is not a single number when a book holds
        // more than one currency.
        let mut per_cur: BTreeMap<String, (Minor, Minor)> = BTreeMap::new();
        for c in cells {
            let e = per_cur.entry(c.currency.clone()).or_insert((0, 0));
            if c.kind == "expense" {
                e.0 += c.amount; // expenses are already positive in ledger convention
            } else {
                e.1 -= c.amount; // income postings are negative; flip to a magnitude
            }
        }
        months.push(serde_json::json!({
            "month": ym,
            "totals": per_cur.iter().map(|(cur, (spent, received))| serde_json::json!({
                "currency": cur,
                "spent_minor": spent,     "spent_decimal": sc.dec(*spent, cur),
                "received_minor": received, "received_decimal": sc.dec(*received, cur),
                "net_minor": received - spent,
                "net_decimal": sc.dec(received - spent, cur),
            })).collect::<Vec<_>>(),
            "by_account": cells.iter().map(|c| serde_json::json!({
                "account": c.account, "kind": c.kind, "currency": c.currency,
                "amount_minor": c.amount, "amount_decimal": sc.dec(c.amount, &c.currency),
            })).collect::<Vec<_>>(),
        }));
    }
    Ok((months, buckets))
}

/// What normal looks like, and how far the latest month is from it.
///
/// The median rather than the mean, because one annual insurance payment drags a mean far enough to
/// make every other month look frugal. `months_observed` travels with each row so a "baseline" from
/// two months is visibly not one.
fn typical(
    sc: &Scales,
    buckets: &BTreeMap<String, Vec<MonthCell>>,
) -> Vec<serde_json::Value> {
    let mut series: BTreeMap<(String, String, String), Vec<Minor>> = BTreeMap::new();
    let mut latest: BTreeMap<(String, String, String), Minor> = BTreeMap::new();
    for (_, cells) in buckets.iter() {
        for c in cells {
            let key = (c.account.clone(), c.kind.clone(), c.currency.clone());
            series.entry(key.clone()).or_default().push(c.amount);
            latest.insert(key, c.amount); // buckets iterate in month order, so this ends on the last
        }
    }

    let mut out = Vec::new();
    for ((account, kind, currency), mut values) in series {
        let observed = values.len();
        values.sort_unstable();
        let med = median(&values);
        let last = *latest.get(&(account.clone(), kind.clone(), currency.clone())).unwrap_or(&0);
        let delta = last - med;
        // Percent is only meaningful against a non-zero baseline; a null says so rather than
        // dividing by zero and reporting an infinity as a trend.
        let pct = if med != 0 {
            serde_json::json!(((delta as f64) / (med.abs() as f64) * 100.0).round() as i64)
        } else {
            serde_json::Value::Null
        };
        out.push(serde_json::json!({
            "account": account,
            "kind": kind,
            // Signs follow the ledger, so an income median is negative. Saying which direction it
            // is beats hoping the reader remembers the convention six sections in.
            "direction": if kind == "income" { "money in" } else { "money out" },
            "currency": currency,
            "months_observed": observed,
            "median_monthly_minor": med, "median_monthly_decimal": sc.dec(med, &currency),
            "latest_month_minor": last, "latest_month_decimal": sc.dec(last, &currency),
            "deviation_minor": delta, "deviation_decimal": sc.dec(delta, &currency),
            "deviation_pct": pct,
        }));
    }
    out.sort_by_key(|r| -(r["deviation_minor"].as_i64().unwrap_or(0).abs()));
    out
}

fn largest(
    conn: &Connection,
    sc: &Scales,
    from: NaiveDate,
    to: NaiveDate,
    n: usize,
) -> Result<Vec<serde_json::Value>, AnalysisError> {
    let mut st = conn.prepare(
        "SELECT t.id, t.occurred_on, t.description, t.payee, a.name, p.currency, p.amount_minor
           FROM posting p
           JOIN txn t     ON t.id = p.txn_id
           JOIN account a ON a.id = p.account_id
          WHERE a.kind = 'expense' AND t.occurred_on >= ?1 AND t.occurred_on <= ?2
          ORDER BY p.amount_minor DESC
          LIMIT ?3",
    )?;
    let rows = st
        .query_map(rusqlite::params![from.to_string(), to.to_string(), n as i64], |r| {
            let cur: String = r.get(5)?;
            let amt: Minor = r.get(6)?;
            Ok(serde_json::json!({
                "txn_id": r.get::<_, i64>(0)?,
                "on_date": r.get::<_, String>(1)?,
                "description": r.get::<_, String>(2)?,
                "payee": r.get::<_, Option<String>>(3)?,
                "account": r.get::<_, String>(4)?,
                "currency": cur, "amount_minor": amt, "amount_decimal": sc.dec(amt, &cur),
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The forward view, reduced to the three things anyone actually asks a forecast.
///
/// Not the whole balance curve: "how low does it get, when, and does it go under" is the question,
/// and a model handed 180 daily balances will answer it by scanning and mis-scanning.
fn outlook(
    conn: &Connection,
    sc: &Scales,
    as_of: NaiveDate,
    horizon: NaiveDate,
) -> Result<serde_json::Value, AnalysisError> {
    let snap = forecast::load::snapshot(conn, as_of)?;
    let proj = forecast::project(&RRuleCrate, &snap, as_of, horizon, &HashSet::new())
        .map_err(|e| AnalysisError::Bad(e.to_string()))?;

    let mut kinds: HashMap<String, String> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT name, kind FROM account WHERE system = 0")?;
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (n, k) = row?;
            kinds.insert(n, k);
        }
    }

    struct Track {
        currency: String,
        min: Minor,
        min_on: String,
        closing: Minor,
        closing_on: String,
        first_negative: Option<String>,
    }
    let mut tracks: BTreeMap<String, Track> = BTreeMap::new();
    for b in &proj.balances {
        if !matches!(kinds.get(&b.account).map(String::as_str), Some("asset") | Some("liability")) {
            continue;
        }
        let t = tracks.entry(b.account.clone()).or_insert_with(|| Track {
            currency: b.currency.clone(),
            min: b.balance_minor,
            min_on: b.on.clone(),
            closing: b.balance_minor,
            closing_on: b.on.clone(),
            first_negative: None,
        });
        if b.balance_minor < t.min {
            t.min = b.balance_minor;
            t.min_on = b.on.clone();
        }
        // Balances arrive in date order, so the last write is the closing one.
        t.closing = b.balance_minor;
        t.closing_on = b.on.clone();
        if b.balance_minor < 0 && t.first_negative.is_none() {
            t.first_negative = Some(b.on.clone());
        }
    }

    let accounts: Vec<serde_json::Value> = tracks
        .iter()
        .map(|(name, t)| {
            serde_json::json!({
                "account": name, "currency": t.currency,
                "lowest_minor": t.min, "lowest_decimal": sc.dec(t.min, &t.currency),
                "lowest_on": t.min_on,
                "closing_minor": t.closing, "closing_decimal": sc.dec(t.closing, &t.currency),
                "closing_on": t.closing_on,
                "first_negative_on": t.first_negative,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "derivation": "the same projection the forecast chart draws, baseline scenarios only, \
                       reduced to each account's low point, closing balance and first negative day",
        "as_of": as_of.to_string(),
        "horizon": horizon.to_string(),
        "accounts": accounts,
        "projected_movements": proj.occurrences.len(),
        "goes_negative": accounts.iter().any(|a| !a["first_negative_on"].is_null()),
        "note": "Accounts with no projected movement in the window are absent -- absence means \
                 'nothing scheduled', not 'balance zero'. See position for where they stand today.",
    }))
}

/// What this book cannot tell you. The most important section, and the one most likely to be
/// skimmed, so each entry is phrased as the wrong conclusion it prevents.
fn limits(
    conn: &Connection,
    sc: &Scales,
    as_of: NaiveDate,
    window_from: NaiveDate,
    complete_months: usize,
) -> Result<Vec<serde_json::Value>, AnalysisError> {
    let mut out = Vec::new();
    let mut add = |severity: &str, what: &str, detail: String| {
        out.push(serde_json::json!({ "severity": severity, "limit": what, "detail": detail }));
    };

    let span: (Option<String>, Option<String>, i64) = conn.query_row(
        "SELECT MIN(occurred_on), MAX(occurred_on), COUNT(*) FROM txn",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    match span.0 {
        None => add(
            "fatal",
            "the book is empty",
            "There are no transactions. Every section below is vacuous; do not report zeroes as findings."
                .into(),
        ),
        Some(ref first) => {
            add(
                "always",
                "history starts here",
                format!(
                    "The earliest transaction is {first} and the latest is {}. \"Never\" and \"first time\" \
                     can only mean \"not since {first}\". {} transactions in total.",
                    span.1.clone().unwrap_or_default(),
                    span.2
                ),
            );
        }
    }

    if complete_months < 3 {
        add(
            "high",
            "no baseline yet",
            format!(
                "Only {complete_months} complete calendar month(s) sit in the window from {window_from}. \
                 A median over that is not a baseline -- report levels, not trends, and do not call \
                 anything unusual."
            ),
        );
    }

    // Unclassified spend is the one that silently invalidates every category conclusion.
    //
    // Found by ID, not by the name it was created with. Matching the string was how this caveat
    // could be switched OFF by a rename: the money stayed exactly where it was and stayed exactly as
    // uncategorised, but the brief stopped saying so, and every category total it prints kept being
    // wrong by that much with nothing left to warn the reader. The account is also NAMED by whatever
    // it is called now, so the caveat points at something the operator can actually find.
    let bucket = crate::roles::unclassified_existing(conn);
    let unclassified: Vec<(String, Minor)> = match bucket.as_ref() {
        // A book that has never imported anything has no bucket, which is an absence of
        // uncategorised spend rather than a missing measurement.
        None => Vec::new(),
        Some((id, _)) => {
            let mut st = conn.prepare(
                "SELECT p.currency, SUM(p.amount_minor)
                   FROM posting p JOIN txn t ON t.id = p.txn_id
                  WHERE p.account_id = ?1 AND t.occurred_on >= ?2
                  GROUP BY p.currency",
            )?;
            let rows = st
                .query_map(rusqlite::params![id, window_from.to_string()], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<Result<_, _>>()?;
            rows
        }
    };
    let bucket_name =
        bucket.map(|(_, n)| n).unwrap_or_else(|| crate::roles::UNCLASSIFIED_NAME.to_string());
    let mut st = conn.prepare(
        "SELECT p.currency, SUM(p.amount_minor)
           FROM posting p JOIN account a ON a.id = p.account_id
           JOIN txn t ON t.id = p.txn_id
          WHERE a.kind = 'expense' AND t.occurred_on >= ?1
          GROUP BY p.currency",
    )?;
    let all_expense: HashMap<String, Minor> = st
        .query_map(rusqlite::params![window_from.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (cur, amount) in unclassified {
        let total = *all_expense.get(&cur).unwrap_or(&0);
        let pct = if total != 0 { amount * 100 / total } else { 0 };
        add(
            if pct >= 10 { "high" } else { "medium" },
            "spending that has no category",
            format!(
                "{} {cur} ({pct}% of expenditure since {window_from}) sits in {bucket_name}. \
                 Category totals and medians are wrong by up to that much, in an unknown direction.",
                sc.dec(amount, &cur)
            ),
        );
    }

    let series_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM series WHERE scenario_id IS NULL", [], |r| r.get(0))?;
    if series_count == 0 {
        add(
            "high",
            "nothing recurring is recorded",
            "No baseline series exist, so `commitments` is empty and `outlook` projects nothing. \
             That is a gap in the book, not an absence of commitments in real life."
                .into(),
        );
    }

    // A currency in use with no recent rate makes any conversion silently stale.
    let mut st = conn.prepare(
        "SELECT DISTINCT p.currency FROM posting p
          WHERE p.currency <> (SELECT value FROM book_meta WHERE key = 'display_currency')",
    )?;
    let foreign: Vec<String> =
        st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for cur in foreign {
        let latest: Option<String> = conn
            .query_row(
                "SELECT MAX(as_of) FROM fx_rate WHERE quote_code = ?1 OR base_code = ?1",
                rusqlite::params![cur],
                |r| r.get(0),
            )
            .unwrap_or(None);
        let stale = match &latest {
            None => true,
            Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map(|d| (as_of - d).num_days() > 30)
                .unwrap_or(true),
        };
        if stale {
            add(
                "medium",
                "a foreign currency has no fresh rate",
                format!(
                    "{cur} is held in this book but its most recent stored rate is {}. Any figure \
                     converted into the display currency is that stale, and no conversion appears \
                     in this brief at all.",
                    latest.unwrap_or_else(|| "absent".into())
                ),
            );
        }
    }

    let unbalanced: i64 =
        conn.query_row("SELECT COUNT(*) FROM v_check_txn_unbalanced", [], |r| r.get(0))?;
    if unbalanced > 0 {
        add(
            "fatal",
            "the book does not balance",
            format!(
                "{unbalanced} transaction(s) have postings that do not sum to zero within a currency. \
                 The book is corrupt; nothing here can be trusted until db.check is clean."
            ),
        );
    }

    add(
        "always",
        "this brief is not the ledger",
        "It summarises. To answer anything at the level of an individual payment -- who, when, how \
         often, which of these was the odd one -- call analysis.query with SQL rather than \
         inferring from these totals."
            .into(),
    );

    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// the brief
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct BriefOptions {
    pub as_of: NaiveDate,
    /// Complete calendar months of history to summarise.
    pub months: u32,
    /// How far forward `outlook` projects.
    pub horizon: NaiveDate,
    pub largest_n: usize,
}

pub fn brief(conn: &Connection, opt: &BriefOptions) -> Result<serde_json::Value, AnalysisError> {
    let sc = Scales(minor_digits(conn)?);

    // The window ends at the last day of the LAST COMPLETE month. Including the month in progress
    // is the single most reliable way to manufacture a downward trend that does not exist.
    let this_month = month_start(opt.as_of);
    let window_to = this_month.pred_opt().unwrap_or(opt.as_of);
    let window_from = shift_months(this_month, -(opt.months as i32));
    let complete = if window_to < window_from { 0 } else { opt.months as usize };

    let (months, buckets) = monthly(conn, &sc, window_from, window_to)?;
    let observed = months.len();

    Ok(serde_json::json!({
        "format": "minka-ledger brief v1",
        "as_of": opt.as_of.to_string(),
        "read_this_first": BRIEF_PREAMBLE,
        "history_window": {
            "from": window_from.to_string(),
            "to": window_to.to_string(),
            "complete_months_requested": complete,
            "complete_months_with_data": observed,
            "excludes": format!("{} onwards, the month in progress", this_month),
        },
        "position": position(conn, &sc, opt.as_of)?,
        "commitments": commitments(conn, &sc, opt.as_of)?,
        "months": months,
        "typical": {
            "derivation": "median of the complete monthly totals above, per account per currency; \
                           deviation compares the latest complete month against that median. \
                           Amounts keep the ledger's signs -- expenses positive, income negative -- \
                           so read `direction` rather than the sign. A positive deviation on an \
                           expense means more was spent than usual; on income it means less was \
                           received.",
            "accounts": typical(&sc, &buckets),
        },
        "largest_expenses": largest(conn, &sc, window_from, window_to, opt.largest_n)?,
        "outlook": outlook(conn, &sc, opt.as_of, opt.horizon)?,
        "limits": limits(conn, &sc, opt.as_of, window_from, observed)?,
    }))
}

// ---------------------------------------------------------------------------------------------
// drill-down
// ---------------------------------------------------------------------------------------------

/// Rows the reader is allowed back from one query. A cap exists so a model asking for "everything"
/// gets a truncation flag rather than a context overflow it will not notice.
pub const MAX_ROWS: usize = 2000;

/// How many SQLite VM steps a query may burn before it is interrupted. A cartesian join across
/// posting and txn is easy to write by accident and would otherwise wedge the core, which is a
/// single-threaded stdio server with nobody to cancel it.
const STEP_BUDGET: usize = 200_000;

/// Run read-only SQL against the book.
///
/// The connection is opened `SQLITE_OPEN_READ_ONLY`, so this is not a matter of validating the
/// statement -- writes are refused by SQLite itself, and a caller cannot smuggle one past a parser
/// this module does not have. That is the whole reason for a second connection rather than reusing
/// the server's: `query_only` would have to be set and unset around every call, and a panic in
/// between leaves the book in whichever state the bug chose.
pub fn query(
    path: &str,
    sql: &str,
    limit: usize,
) -> Result<serde_json::Value, AnalysisError> {
    use rusqlite::OpenFlags;
    let limit = limit.clamp(1, MAX_ROWS);
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;

    let steps = std::cell::Cell::new(0usize);
    // The handler runs every 1000 VM instructions; returning true aborts the statement.
    conn.progress_handler(1000, Some(move || {
        steps.set(steps.get() + 1000);
        steps.get() > STEP_BUDGET
    }))?;

    let mut st = conn.prepare(sql).map_err(|e| AnalysisError::Bad(e.to_string()))?;
    let columns: Vec<String> = st.column_names().into_iter().map(str::to_string).collect();
    let width = columns.len();

    let mut rows = Vec::new();
    let mut cursor = st.query([]).map_err(|e| AnalysisError::Bad(e.to_string()))?;
    let mut truncated = false;
    loop {
        match cursor.next() {
            Ok(Some(r)) => {
                if rows.len() >= limit {
                    truncated = true;
                    break;
                }
                let mut row = Vec::with_capacity(width);
                for i in 0..width {
                    row.push(match r.get_ref(i) {
                        Ok(rusqlite::types::ValueRef::Null) => serde_json::Value::Null,
                        Ok(rusqlite::types::ValueRef::Integer(v)) => serde_json::json!(v),
                        Ok(rusqlite::types::ValueRef::Real(v)) => serde_json::json!(v),
                        Ok(rusqlite::types::ValueRef::Text(v)) => {
                            serde_json::json!(String::from_utf8_lossy(v))
                        }
                        Ok(rusqlite::types::ValueRef::Blob(v)) => {
                            serde_json::json!(format!("<{} bytes>", v.len()))
                        }
                        Err(_) => serde_json::Value::Null,
                    });
                }
                rows.push(serde_json::Value::Array(row));
            }
            Ok(None) => break,
            Err(e) => return Err(AnalysisError::Bad(e.to_string())),
        }
    }

    Ok(serde_json::json!({
        "columns": columns,
        "rows": rows,
        "row_count": rows.len(),
        "truncated": truncated,
        "note": if truncated {
            format!("stopped at {limit} rows -- add LIMIT, or aggregate, rather than assuming this is all of them")
        } else {
            "complete result".to_string()
        },
    }))
}

/// The DDL, so a model can write SQL that is correct on the first attempt rather than the third.
///
/// The comments in the schema are kept: they carry the reasoning that the column names do not, and
/// they are the difference between a reader that knows `occurrence_on` is a slot identity and one
/// that treats it as a date.
pub fn schema(conn: &Connection) -> Result<serde_json::Value, AnalysisError> {
    let mut st = conn.prepare(
        "SELECT type, name, sql FROM sqlite_master
          WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
          ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 1 ELSE 2 END, name",
    )?;
    let objects: Vec<serde_json::Value> = st
        .query_map([], |r| {
            Ok(serde_json::json!({
                "type": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "sql": r.get::<_, String>(2)?,
            }))
        })?
        .collect::<Result<_, _>>()?;

    Ok(serde_json::json!({
        "read_this_first": crate::export::PREAMBLE,
        "start_here": [
            "v_ledger_line is one row per POSTING with the account, the date, a pre-formatted \
             amount_decimal and the series it came from. It is the right table for almost every \
             question about what happened.",
            "v_export_facts is one row per account per currency with its closing balance.",
            "Aggregate in SQL. SUM(), GROUP BY and strftime('%Y-%m', on_date) are exact here and \
             are not exact when done by reading rows.",
            "The three v_check_* views must return zero rows. If any returns a row, stop.",
            "A recurring chain is several series rows sharing chain_id, one per leg; when summing \
             commitments count only chain_seq = 0 (or NULL), or the same money is counted once \
             per leg.",
        ],
        "objects": objects,
    }))
}

/// A machine-readable description of what an agent may call. Handed to a model up front, this is
/// the difference between one that asks for a data dump and one that asks a question.
pub fn tools() -> serde_json::Value {
    serde_json::json!({
        "protocol": "NDJSON over stdio: send {\"id\":1,\"method\":\"...\",\"params\":{...}}\\n, \
                     read one {\"id\":1,\"result\":...} or {\"id\":1,\"error\":{...}} line back.",
        "suggested_order": [
            "analysis.brief once, to get the computed picture and its limits",
            "analysis.schema if you intend to query",
            "analysis.query as many times as needed, to answer specifics",
        ],
        "tools": [
            {
                "name": "analysis.brief",
                "description": "The computed financial picture: balances, what recurs and its true \
                                monthly cost, complete-month history, medians and deviations, the \
                                largest expenses, the forward outlook, and what the book cannot tell you.",
                "params": {
                    "as_of": "YYYY-MM-DD, default today",
                    "months": "complete months of history, default 6",
                    "horizon": "YYYY-MM-DD to project to, default as_of + 6 months",
                    "largest": "how many largest expenses, default 10"
                }
            },
            {
                "name": "analysis.query",
                "description": "Read-only SQL against the book. The connection is opened read-only, \
                                so writes are refused by the database, not by a filter.",
                "params": { "sql": "one SELECT statement", "limit": "rows, default 200, max 2000" }
            },
            {
                "name": "analysis.schema",
                "description": "Every table and view with its DDL and comments, plus which views to \
                                start from.",
                "params": {}
            },
            {
                "name": "forecast.project",
                "description": "The full projection, including scenario overlays, when the brief's \
                                summarised outlook is not enough.",
                "params": { "as_of": "YYYY-MM-DD", "horizon": "YYYY-MM-DD", "scenarios": "[id, ...]" }
            },
            {
                "name": "export.bundle",
                "description": "The whole ledger as JSON. Large. Prefer analysis.query unless you \
                                genuinely need every line.",
                "params": { "from": "YYYY-MM-DD", "to": "YYYY-MM-DD", "redact": "bool" }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{self, NewPosting, NewTxn};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// A book with five complete months of history and one recurring commitment, so the
    /// complete-month and median rules have something to be right or wrong about.
    fn book() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES
               (1,'Current','asset','GBP'), (2,'Salary','income','GBP'),
               (3,'Groceries','expense','GBP'), (4,'Rent','expense','GBP'),
               (5,'Expenses:Unclassified','expense','GBP'), (6,'Yen pot','asset','JPY');",
        )
        .unwrap();

        // March..July: salary in, rent out, groceries varying around 300.
        let groceries = [28_000, 31_000, 30_000, 55_000, 29_000];
        for (i, month) in ["03", "04", "05", "06", "07"].iter().enumerate() {
            entry::create(&mut conn, &NewTxn {
                occurred_on: format!("2026-{month}-01"),
                description: "Salary".into(), payee: None, note: None,
                postings: vec![NewPosting { account_id: 1, amount_minor: 250_000 },
                               NewPosting { account_id: 2, amount_minor: -250_000 }],
                series_id: None,
                occurrence_on: None,
            }).unwrap();
            entry::create(&mut conn, &NewTxn {
                occurred_on: format!("2026-{month}-02"),
                description: "Rent".into(), payee: None, note: None,
                postings: vec![NewPosting { account_id: 1, amount_minor: -90_000 },
                               NewPosting { account_id: 4, amount_minor: 90_000 }],
                series_id: None,
                occurrence_on: None,
            }).unwrap();
            entry::create(&mut conn, &NewTxn {
                occurred_on: format!("2026-{month}-15"),
                description: "Groceries".into(), payee: None, note: None,
                postings: vec![NewPosting { account_id: 1, amount_minor: -groceries[i] },
                               NewPosting { account_id: 3, amount_minor: groceries[i] }],
                series_id: None,
                occurrence_on: None,
            }).unwrap();
        }
        conn
    }

    fn with_rent_series(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO series(id,description,rrule,dtstart) VALUES
               (1,'Rent','FREQ=MONTHLY;BYMONTHDAY=2','2026-01-02');
             INSERT INTO series_posting(series_id,account_id,currency,amount_minor,role)
               VALUES(1,1,'GBP',-90000,'primary'),(1,4,'GBP',90000,'balancing');",
        )
        .unwrap();
    }

    fn opts(as_of: &str) -> BriefOptions {
        BriefOptions {
            as_of: d(as_of),
            months: 6,
            horizon: d("2027-02-01"),
            largest_n: 5,
        }
    }

    #[test]
    fn the_month_in_progress_is_excluded_from_history() {
        let c = book();
        // as_of is mid-August; August must not appear, and the window must end 31 July.
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        assert_eq!(b["history_window"]["to"], "2026-07-31");
        let months: Vec<&str> =
            b["months"].as_array().unwrap().iter().map(|m| m["month"].as_str().unwrap()).collect();
        assert!(!months.contains(&"2026-08"), "a part month was compared against whole ones: {months:?}");
        assert_eq!(months.last(), Some(&"2026-07"));
    }

    #[test]
    fn spent_and_received_are_positive_magnitudes() {
        let c = book();
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let july = b["months"].as_array().unwrap().iter()
            .find(|m| m["month"] == "2026-07").unwrap();
        let t = &july["totals"][0];
        assert_eq!(t["currency"], "GBP");
        assert_eq!(t["received_minor"], 250_000, "income is stored negative and must be flipped");
        assert_eq!(t["spent_minor"], 119_000, "rent 900 + groceries 290");
        assert_eq!(t["net_minor"], 131_000);
        assert_eq!(t["spent_decimal"], "1190.00");
    }

    #[test]
    fn the_median_ignores_the_one_expensive_month() {
        let c = book();
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let g = b["typical"]["accounts"].as_array().unwrap().iter()
            .find(|a| a["account"] == "Groceries").unwrap();
        // 280 290 300 310 550 -> median 300, mean would be 346
        assert_eq!(g["median_monthly_minor"], 30_000, "a mean would be dragged by the 550 month");
        assert_eq!(g["months_observed"], 5);
        assert_eq!(g["latest_month_minor"], 29_000);
        assert_eq!(g["deviation_minor"], -1_000);
    }

    #[test]
    fn four_weekly_costs_thirteen_twelfths_of_monthly() {
        let c = book();
        c.execute_batch(
            "INSERT INTO series(id,description,rrule,dtstart) VALUES
               (1,'Monthly thing','FREQ=MONTHLY;BYMONTHDAY=5','2026-01-05'),
               (2,'Four-weekly thing','FREQ=WEEKLY;INTERVAL=4;BYDAY=MO','2026-01-05');
             INSERT INTO series_posting(series_id,account_id,currency,amount_minor,role)
               VALUES(1,1,'GBP',-10000,'primary'),(1,3,'GBP',10000,'balancing'),
                     (2,1,'GBP',-10000,'primary'),(2,3,'GBP',10000,'balancing');",
        ).unwrap();
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let s = b["commitments"]["series"].as_array().unwrap();
        let m = s.iter().find(|r| r["description"] == "Monthly thing").unwrap();
        let f = s.iter().find(|r| r["description"] == "Four-weekly thing").unwrap();

        assert_eq!(m["occurrences_next_12m"], 12);
        assert_eq!(m["monthly_equivalent_minor"], -10_000);
        assert_eq!(f["occurrences_next_12m"], 13, "4-weekly lands 13 times in a year");
        // The naive answer -- same amount, so same monthly cost -- is the bug this guards.
        assert_ne!(f["monthly_equivalent_minor"], m["monthly_equivalent_minor"]);
        assert_eq!(f["monthly_equivalent_minor"], -10_833, "13/12 of 100.00, rounded half away");
    }

    #[test]
    fn commitments_count_one_leg_only() {
        let c = book();
        with_rent_series(&c);
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let rows = b["commitments"]["series"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "both legs of one series would net to zero and show as two rows");
        assert_eq!(rows[0]["monthly_equivalent_minor"], -90_000);
        assert_eq!(b["commitments"]["monthly_equivalent_by_currency"][0]["amount_minor"], -90_000);
    }

    #[test]
    fn the_outlook_names_the_low_point_and_when_it_arrives() {
        let c = book();
        with_rent_series(&c);
        // Only rent is a series -- no salary -- so the balance falls 900.00 a month from 6270.00.
        // Six months does not exhaust it and eleven does, which is exactly the distinction the
        // outlook exists to make: the same book is solvent or not depending on how far you look.
        let near = brief(&c, &BriefOptions { horizon: d("2027-02-01"), ..opts("2026-08-15") }).unwrap();
        assert_eq!(near["outlook"]["goes_negative"], false);

        let far = brief(&c, &BriefOptions { horizon: d("2027-12-01"), ..opts("2026-08-15") }).unwrap();
        assert_eq!(far["outlook"]["goes_negative"], true);
        let acct = far["outlook"]["accounts"].as_array().unwrap().iter()
            .find(|a| a["account"] == "Current").unwrap();
        assert_eq!(acct["first_negative_on"], "2027-03-02", "the date it goes under is the question");
        assert_eq!(acct["lowest_on"], acct["closing_on"], "a falling balance bottoms out at the end");
    }

    #[test]
    fn unclassified_spending_is_reported_as_a_limit_on_the_analysis() {
        let mut c = book();
        entry::create(&mut c, &NewTxn {
            occurred_on: "2026-07-20".into(), description: "Card payment".into(),
            payee: None, note: None,
            postings: vec![NewPosting { account_id: 1, amount_minor: -100_000 },
                           NewPosting { account_id: 5, amount_minor: 100_000 }],
            series_id: None,
            occurrence_on: None,
        }).unwrap();
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let l = b["limits"].as_array().unwrap().iter()
            .find(|l| l["limit"] == "spending that has no category")
            .expect("unclassified spend must be declared, not silently averaged in");
        // 1000.00 unclassified against 7230.00 of expenditure -- 13%, enough to move a category
        // total more than most of the conclusions anyone would draw from one.
        assert_eq!(l["severity"], "high");
        assert!(l["detail"].as_str().unwrap().contains("1000.00"), "the figure itself must be shown");
        assert!(l["detail"].as_str().unwrap().contains("13%"));
    }

    /// The caveat used to be keyed on the account's literal NAME, so `account.rename` could switch
    /// it off: the money stayed exactly where it was and stayed exactly as uncategorised, but the
    /// brief stopped saying so and every category total it printed went on being wrong by that much
    /// with nothing left to warn the reader. Nothing is left behind as a trace either -- unlike the
    /// equity case there is no second account to notice.
    #[test]
    fn renaming_the_unclassified_bucket_does_not_silence_its_caveat() {
        let mut c = book();
        entry::create(&mut c, &NewTxn {
            occurred_on: "2026-07-20".into(), description: "Card payment".into(),
            payee: None, note: None,
            postings: vec![NewPosting { account_id: 1, amount_minor: -100_000 },
                           NewPosting { account_id: 5, amount_minor: 100_000 }],
            series_id: None,
            occurrence_on: None,
        }).unwrap();
        // The fixture builds the book at 0001 and inserts its accounts afterwards, so run the pin
        // backfill here -- which is exactly the order a real upgrade sees it in: db::migrate runs
        // 0003 against a book whose accounts already exist, before any handler can rename one.
        c.execute_batch(include_str!("../migrations/0003_role_account_pins.sql")).unwrap();
        c.execute("UPDATE account SET name = 'Misc spending' WHERE id = 5", []).unwrap();

        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let l = b["limits"].as_array().unwrap().iter()
            .find(|l| l["limit"] == "spending that has no category")
            .expect("a rename must not be able to switch the caveat off");
        assert_eq!(l["severity"], "high");
        let detail = l["detail"].as_str().unwrap();
        assert!(detail.contains("1000.00"), "the figure is unchanged by a rename: {detail}");
        // Named by what it is called NOW, or the caveat points at an account nobody can find.
        assert!(detail.contains("Misc spending"), "{detail}");
        assert!(!detail.contains("Expenses:Unclassified"), "{detail}");
    }

    #[test]
    fn a_thin_book_says_it_has_no_baseline() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES
               (1,'Current','asset','GBP'), (3,'Groceries','expense','GBP');",
        ).unwrap();
        entry::create(&mut c, &NewTxn {
            occurred_on: "2026-07-15".into(), description: "Shop".into(), payee: None, note: None,
            postings: vec![NewPosting { account_id: 1, amount_minor: -1_000 },
                           NewPosting { account_id: 3, amount_minor: 1_000 }],
            series_id: None,
            occurrence_on: None,
        }).unwrap();
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let kinds: Vec<&str> =
            b["limits"].as_array().unwrap().iter().map(|l| l["limit"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"no baseline yet"), "one month is not a baseline: {kinds:?}");
        assert!(kinds.contains(&"nothing recurring is recorded"));
        assert!(kinds.contains(&"history starts here"));
    }

    #[test]
    fn an_empty_book_is_called_empty_rather_than_reported_as_zeroes() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrate(&c).unwrap();
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let fatal = b["limits"].as_array().unwrap().iter()
            .find(|l| l["severity"] == "fatal").unwrap();
        assert_eq!(fatal["limit"], "the book is empty");
    }

    #[test]
    fn balances_never_net_across_currencies() {
        let mut c = book();
        entry::create(&mut c, &NewTxn {
            occurred_on: "2026-07-03".into(), description: "Yen".into(), payee: None, note: None,
            // A single-currency txn on the JPY account, balanced against itself via a second
            // JPY account would be needed for realism; here the point is only that the brief
            // reports JPY separately.
            postings: vec![NewPosting { account_id: 6, amount_minor: 100_000 },
                           NewPosting { account_id: 6, amount_minor: -100_000 }],
            series_id: None,
            occurrence_on: None,
        }).unwrap();
        let b = brief(&c, &opts("2026-08-15")).unwrap();
        let net = b["position"]["net_by_currency"].as_array().unwrap();
        let codes: Vec<&str> = net.iter().map(|n| n["currency"].as_str().unwrap()).collect();
        assert!(codes.contains(&"GBP") && codes.contains(&"JPY"));
        assert!(b["position"].get("net_total").is_none(), "a cross-currency total must not exist");
    }

    #[test]
    fn jpy_is_formatted_with_no_decimal_places() {
        let sc = Scales(minor_digits(&book()).unwrap());
        assert_eq!(sc.dec(100_000, "JPY"), "100000", "dividing JPY by 100 is a 100x error");
        assert_eq!(sc.dec(100_000, "GBP"), "1000.00");
    }

    #[test]
    fn the_preamble_names_the_traps_that_matter_for_computed_figures() {
        let all = BRIEF_PREAMBLE.join(" ");
        assert!(all.contains("Do not re-derive"));
        assert!(all.contains("COMPLETE calendar months"));
        assert!(all.contains("twelve-over-period"), "the annualisation trap must be stated");
        assert!(all.contains("Read `limits`"));
    }

    // ---- drill-down ----

    /// `name` must differ per test: these run as threads of ONE process, so a path keyed on the
    /// pid is the same path for all of them and VACUUM INTO refuses an existing file.
    fn on_disk(name: &str) -> (Connection, String) {
        let dir = std::env::temp_dir().join(format!("minka-analysis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.db")).to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let src = book();
        src.execute("VACUUM INTO ?1", [&path]).unwrap();
        (src, path)
    }

    #[test]
    fn a_query_returns_columns_and_rows() {
        let (_src, path) = on_disk("columns");
        let r = query(&path, "SELECT account, SUM(amount_minor) FROM v_ledger_line \
                              WHERE account_kind='expense' GROUP BY account ORDER BY account", 100)
            .unwrap();
        assert_eq!(r["columns"][0], "account");
        assert_eq!(r["truncated"], false);
        let rows = r["rows"].as_array().unwrap();
        let groceries = rows.iter().find(|r| r[0] == "Groceries").unwrap();
        assert_eq!(groceries[1], 173_000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_write_is_refused_by_the_database_itself() {
        let (_src, path) = on_disk("readonly");
        let e = query(&path, "DELETE FROM txn", 100).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("readonly") || msg.contains("read-only"), "got: {msg}");
        // and the book is untouched
        let n = query(&path, "SELECT COUNT(*) FROM txn", 10).unwrap();
        assert_eq!(n["rows"][0][0], 15);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncation_is_flagged_rather_than_silent() {
        let (_src, path) = on_disk("truncate");
        let r = query(&path, "SELECT * FROM v_ledger_line", 3).unwrap();
        assert_eq!(r["row_count"], 3);
        assert_eq!(r["truncated"], true, "a silently short answer is the worst outcome here");
        assert!(r["note"].as_str().unwrap().contains("stopped at 3 rows"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_runaway_query_is_interrupted_rather_than_hanging_the_core() {
        let (_src, path) = on_disk("runaway");
        // A four-way cartesian join over the posting table, aggregated so no row cap can stop it.
        // 30 postings joined six ways is 7.3e8 rows -- and it is AGGREGATED, so no row cap can
        // stop it. Only the step budget can.
        let e = query(
            &path,
            "SELECT COUNT(*) FROM posting a, posting b, posting c, posting d, posting e, posting f",
            10,
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("interrupted"),
            "must fail by the step budget, not by chance: {e}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_schema_carries_the_comments_that_explain_the_columns() {
        let c = book();
        let s = schema(&c).unwrap();
        let objects = s["objects"].as_array().unwrap();
        let view = objects.iter().find(|o| o["name"] == "v_ledger_line").unwrap();
        assert_eq!(view["type"], "view");
        assert!(view["sql"].as_str().unwrap().contains("amount_decimal"));
        assert!(objects.iter().any(|o| o["name"] == "posting" && o["type"] == "table"));
        assert!(s["start_here"].as_array().unwrap().iter()
            .any(|h| h.as_str().unwrap().contains("v_ledger_line")));
    }

    #[test]
    fn the_tool_list_tells_an_agent_what_to_call_first() {
        let t = tools();
        let names: Vec<&str> = t["tools"].as_array().unwrap().iter()
            .map(|x| x["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"analysis.brief"));
        assert!(names.contains(&"analysis.query"));
        assert!(t["suggested_order"][0].as_str().unwrap().contains("analysis.brief"));
    }
}
