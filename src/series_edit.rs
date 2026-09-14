//! Reviewing and editing recurring payment rules.
//!
//! WHAT A RULE IS HERE. A recurring payment is stored as series rows, and one rule can be several
//! of them in two independent ways:
//!
//! - a recurring CHAIN is one row per hop, tied by `chain_id` (migrations/0004_series_chain.sql);
//! - a rule changed "from a date" is one PART per stretch of time: the part that was running is
//!   ended the day before, and a new part starts with `head_id` pointing back at the first part's
//!   matching hop (the split migrations/0001_init.sql planned for "this and future" edits).
//!
//! A part is one family (a plain row, or every hop of a chain); a rule is every part sharing
//! `COALESCE(head_id, id)` of its first hop -- `forecast::lineage_map`. Parts never overlap, and
//! only the NEWEST part of a rule can be edited: the older ones are history.
//!
//! WHAT AN EDIT MAY NEVER DO. Every recorded payment (`txn.series_id` + `occurrence_on`) and every
//! adjustment (`series_override`) hangs off a slot: a date the rule itself produced. That key is
//! never rewritten. So:
//!
//! - a change to WHICH dates a rule produces (its rule or its start) is refused for the whole rule
//!   while a recorded payment or a live adjustment sits on a date the new rule would not produce,
//!   because the payment would stop claiming its slot and the month would be counted twice. The
//!   refusal names the dates and offers the change "from a date" instead;
//! - a change "from a date" never moves a recorded payment between parts: one recorded on or
//!   after that date refuses it, with the earliest date that would work;
//! - recorded payments keep their own postings, date and description whatever changes, the same
//!   guarantee `series.rename` makes (a record of what happened is not rewritten by a plan);
//! - nothing is deleted except by `undo_change`, and that only removes a part no payment points at.
//!
//! Every write runs inside one transaction and can be DRY RUN: the writes happen, the result is
//! computed from the database as it would be, and the transaction is rolled back. A dry run
//! reports what would block a save as data rather than as an error, so a form can check as its
//! user types without flashing an error for every half-typed field.

use chrono::{Duration, NaiveDate};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::analysis;
use crate::forecast::{self, Projection, Snapshot, LOOKBACK_DAYS};
use crate::recur::{self, RRuleCrate, Recurrence};

pub(crate) struct EditError {
    pub code: &'static str,
    pub message: String,
}

impl From<rusqlite::Error> for EditError {
    fn from(e: rusqlite::Error) -> Self {
        EditError { code: "sql", message: e.to_string() }
    }
}

fn fail(code: &'static str, message: impl Into<String>) -> EditError {
    EditError { code, message: message.into() }
}

fn bad(message: impl Into<String>) -> EditError {
    fail("bad_params", message)
}

const WEEKEND_RULES: [&str; 4] = ["none", "before", "after", "modified_after"];

/// d/M/yyyy with no leading zeros: how a date is said to the person reading a message.
pub(crate) fn dmy(d: NaiveDate) -> String {
    d.format("%-d/%-m/%Y").to_string()
}

fn day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn dmy_iso(s: &str) -> String {
    day(s).map(dmy).unwrap_or_else(|| s.to_string())
}

fn two_years(d: NaiveDate) -> NaiveDate {
    d + Duration::days(366 * 2)
}

fn list_words(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        n => format!("{} and {}", items[..n - 1].join(", "), items[n - 1]),
    }
}

fn norm_rule(r: &str) -> String {
    r.trim().to_uppercase()
}

// ---------------------------------------------------------------------------------------------
// loading
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Leg {
    id: i64,
    account_id: i64,
    currency: String,
    amount_minor: i64,
    role: String,
}

#[derive(Clone, Debug)]
struct Hop {
    id: i64,
    head_id: Option<i64>,
    description: String,
    rrule: String,
    dtstart: String,
    until_on: Option<String>,
    weekend_rule: String,
    holiday_cal: Option<String>,
    match_window_before: i64,
    match_window_after: i64,
    match_amount_minor: i64,
    match_amount_bp: i64,
    match_auto: i64,
    scenario_id: Option<i64>,
    supersedes_id: Option<i64>,
    chain_id: Option<i64>,
    chain_seq: Option<i64>,
    legs: Vec<Leg>,
}

impl Hop {
    fn start(&self) -> NaiveDate {
        day(&self.dtstart).unwrap_or_default()
    }
    fn until(&self) -> Option<NaiveDate> {
        self.until_on.as_deref().and_then(day)
    }
}

fn load_hops(conn: &Connection) -> rusqlite::Result<Vec<Hop>> {
    let mut st = conn.prepare(
        "SELECT id, head_id, description, rrule, dtstart, until_on, weekend_rule, holiday_cal,
                match_window_before, match_window_after, match_amount_minor, match_amount_bp,
                match_auto, scenario_id, supersedes_id, chain_id, chain_seq
           FROM series ORDER BY id",
    )?;
    let mut hops: Vec<Hop> = st
        .query_map([], |r| {
            Ok(Hop {
                id: r.get(0)?,
                head_id: r.get(1)?,
                description: r.get(2)?,
                rrule: r.get(3)?,
                dtstart: r.get(4)?,
                until_on: r.get(5)?,
                weekend_rule: r.get(6)?,
                holiday_cal: r.get(7)?,
                match_window_before: r.get(8)?,
                match_window_after: r.get(9)?,
                match_amount_minor: r.get(10)?,
                match_amount_bp: r.get(11)?,
                match_auto: r.get(12)?,
                scenario_id: r.get(13)?,
                supersedes_id: r.get(14)?,
                chain_id: r.get(15)?,
                chain_seq: r.get(16)?,
                legs: Vec::new(),
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut st = conn.prepare(
        "SELECT id, series_id, account_id, currency, amount_minor, role FROM series_posting ORDER BY id",
    )?;
    let mut by_series: HashMap<i64, Vec<Leg>> = HashMap::new();
    for row in st.query_map([], |r| {
        Ok((
            r.get::<_, i64>(1)?,
            Leg {
                id: r.get(0)?,
                account_id: r.get(2)?,
                currency: r.get(3)?,
                amount_minor: r.get(4)?,
                role: r.get(5)?,
            },
        ))
    })? {
        let (sid, leg) = row?;
        by_series.entry(sid).or_default().push(leg);
    }
    for h in &mut hops {
        h.legs = by_series.remove(&h.id).unwrap_or_default();
    }
    Ok(hops)
}

/// One stretch of a rule: a plain row, or every hop of a chain in hop order.
#[derive(Clone, Debug)]
struct Part {
    hops: Vec<Hop>,
}

impl Part {
    fn first(&self) -> &Hop {
        &self.hops[0]
    }
    fn ids(&self) -> Vec<i64> {
        self.hops.iter().map(|h| h.id).collect()
    }
    fn lineage(&self) -> i64 {
        self.first().head_id.unwrap_or(self.first().id)
    }
    /// A what-if row that only cancels a rule: it has no postings and moves no money.
    fn is_cancel(&self) -> bool {
        self.first().supersedes_id.is_some() && self.hops.iter().all(|h| h.legs.is_empty())
    }
    fn is_chain(&self) -> bool {
        self.first().chain_id.is_some()
    }
}

fn parts_of(hops: &[Hop]) -> Vec<Part> {
    let mut families: BTreeMap<i64, Vec<Hop>> = BTreeMap::new();
    for h in hops {
        families.entry(h.chain_id.unwrap_or(h.id)).or_default().push(h.clone());
    }
    families
        .into_values()
        .map(|mut hops| {
            hops.sort_by_key(|h| (h.chain_seq.unwrap_or(0), h.id));
            Part { hops }
        })
        .collect()
}

/// Parts grouped by rule, each rule's parts oldest first. The newest part is the last.
fn rules_of(parts: Vec<Part>) -> BTreeMap<i64, Vec<Part>> {
    let mut rules: BTreeMap<i64, Vec<Part>> = BTreeMap::new();
    for p in parts {
        rules.entry(p.lineage()).or_default().push(p);
    }
    for parts in rules.values_mut() {
        parts.sort_by_key(|p| (p.first().start(), p.first().id));
    }
    rules
}

/// The ordinary shape: every hop exactly one primary and one balancing leg of equal and opposite
/// amounts in one currency, and a chain's hops joined end to end. Only this shape can have its
/// amounts and accounts edited, because only this shape is what the form can show; anything else
/// was written by a raw `series.create` and is edited in its name, schedule and end only.
#[derive(Clone, Debug)]
struct Shape {
    two_leg: bool,
    /// [from, stop..., to]
    route: Vec<i64>,
    /// One positive magnitude per hop.
    magnitudes: Vec<i64>,
    /// Per hop: the primary leg's amount is negative (money LEAVES the primary account).
    negative: Vec<bool>,
    currency: String,
}

fn shape_of(part: &Part) -> Shape {
    let currency = part
        .hops
        .iter()
        .find_map(|h| h.legs.first())
        .map(|l| l.currency.clone())
        .unwrap_or_default();
    let mut route = Vec::new();
    let mut magnitudes = Vec::new();
    let mut negative = Vec::new();
    let mut ok = true;
    for (k, h) in part.hops.iter().enumerate() {
        let primary = h.legs.iter().find(|l| l.role == "primary");
        let balancing = h.legs.iter().find(|l| l.role == "balancing");
        let (Some(p), Some(b)) = (primary, balancing) else {
            ok = false;
            break;
        };
        if h.legs.len() != 2
            || p.amount_minor == 0
            || p.amount_minor != -b.amount_minor
            || p.currency != b.currency
            || p.currency != currency
        {
            ok = false;
            break;
        }
        let neg = p.amount_minor < 0;
        let (from, to) = if neg { (p.account_id, b.account_id) } else { (b.account_id, p.account_id) };
        if k == 0 {
            route.push(from);
        } else if route.last() != Some(&from) {
            ok = false;
            break;
        }
        route.push(to);
        magnitudes.push(p.amount_minor.abs());
        negative.push(neg);
    }
    if !ok || part.hops.is_empty() {
        return Shape { two_leg: false, route: Vec::new(), magnitudes: Vec::new(), negative: Vec::new(), currency };
    }
    Shape { two_leg: true, route, magnitudes, negative, currency }
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // id is kept beside the name it belongs to, for debugging a route
struct Acct {
    id: i64,
    name: String,
    kind: String,
    currency: String,
    closed: bool,
    system: bool,
}

fn load_accounts(conn: &Connection) -> rusqlite::Result<HashMap<i64, Acct>> {
    let mut st = conn.prepare("SELECT id, name, kind, currency, closed, system FROM account")?;
    let rows = st.query_map([], |r| {
        Ok(Acct {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            currency: r.get(3)?,
            closed: r.get::<_, i64>(4)? == 1,
            system: r.get::<_, i64>(5)? == 1,
        })
    })?;
    let mut out = HashMap::new();
    for a in rows {
        let a = a?;
        out.insert(a.id, a);
    }
    Ok(out)
}

fn load_scenarios(conn: &Connection) -> rusqlite::Result<HashMap<i64, String>> {
    let mut st = conn.prepare("SELECT id, name FROM scenario")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect()
}

#[derive(Clone, Debug)]
struct Claim {
    txn_id: i64,
    series_id: i64,
    occurrence_on: String,
    occurred_on: String,
}

fn claims_for(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<Claim>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare(&format!(
        "SELECT id, series_id, occurrence_on, occurred_on FROM txn
          WHERE series_id IN ({}) AND occurrence_on IS NOT NULL
          ORDER BY occurrence_on, series_id, id",
        id_list(ids)
    ))?;
    let rows = st.query_map([], |r| {
        Ok(Claim { txn_id: r.get(0)?, series_id: r.get(1)?, occurrence_on: r.get(2)?, occurred_on: r.get(3)? })
    })?;
    rows.collect()
}

#[derive(Clone, Debug)]
struct Adjustment {
    series_id: i64,
    occurrence_on: String,
    action: String,
    moved_to: Option<String>,
    amount_minor: Option<i64>,
    description: Option<String>,
    note: Option<String>,
}

fn adjustments_for(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<Adjustment>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare(&format!(
        "SELECT series_id, occurrence_on, action, moved_to, amount_minor, description, note
           FROM series_override WHERE series_id IN ({}) ORDER BY occurrence_on, series_id",
        id_list(ids)
    ))?;
    let rows = st.query_map([], |r| {
        Ok(Adjustment {
            series_id: r.get(0)?,
            occurrence_on: r.get(1)?,
            action: r.get(2)?,
            moved_to: r.get(3)?,
            amount_minor: r.get(4)?,
            description: r.get(5)?,
            note: r.get(6)?,
        })
    })?;
    rows.collect()
}

fn id_list(ids: &[i64]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

/// What an adjustment did to its slot, in a word a person uses.
fn action_word(a: &Adjustment) -> &'static str {
    if a.action == "skip" {
        "skipped"
    } else if a.moved_to.is_some() && a.amount_minor.is_none() {
        "moved"
    } else if a.moved_to.is_some() {
        "moved and amended"
    } else {
        "amended"
    }
}

// ---------------------------------------------------------------------------------------------
// projection of one rule
// ---------------------------------------------------------------------------------------------

/// The part of a snapshot that belongs to `members`, and nothing else: one rule projected on its
/// own, so a rule that cannot expand only ever breaks its own row and never the others'.
fn sub_snapshot(snap: &Snapshot, members: &HashSet<i64>) -> Snapshot {
    Snapshot {
        opening: BTreeMap::new(),
        account_currency: snap.account_currency.clone(),
        account_name: snap.account_name.clone(),
        series: snap.series.iter().filter(|s| members.contains(&s.id)).cloned().collect(),
        overrides: snap
            .overrides
            .iter()
            .filter(|((sid, _), _)| members.contains(sid))
            .map(|(k, v)| (*k, v.clone()))
            .collect(),
        claimed: snap.claimed.iter().filter(|(sid, _)| members.contains(sid)).copied().collect(),
        dated: snap
            .dated
            .iter()
            .filter(|t| t.series_id.is_some_and(|id| members.contains(&id)))
            .cloned()
            .collect(),
        holidays: snap.holidays.clone(),
        interest_rules: Vec::new(),
        payment_rules: Vec::new(),
    }
}

fn horizon(as_of: NaiveDate) -> NaiveDate {
    analysis::shift_months(as_of, 12)
}

fn project_rule(
    snap: &Snapshot,
    members: &HashSet<i64>,
    scenario: Option<i64>,
    as_of: NaiveDate,
) -> (Snapshot, Result<Projection, String>) {
    let sub = sub_snapshot(snap, members);
    let active: HashSet<i64> = scenario.into_iter().collect();
    let proj = forecast::project(&RRuleCrate, &sub, as_of, horizon(as_of), &active).map_err(|e| e.to_string());
    (sub, proj)
}

/// One hop's money on one slot, as the forecast has it.
#[derive(Clone, Debug, PartialEq)]
struct Point {
    series_id: i64,
    seq: i64,
    occurrence_on: String,
    value_on: String,
    amount_minor: i64,
    amended: bool,
}

/// Each hop's PRIMARY leg per slot, keyed by (hop position, slot). Positions and slots, not
/// series ids, so the same payment before and after a change from a date is one key whose amount
/// changed rather than one that vanished and one that appeared.
fn points(sub: &Snapshot, proj: &Projection) -> BTreeMap<(i64, String), Point> {
    let primary: HashMap<i64, (i64, i64)> = sub
        .series
        .iter()
        .filter_map(|s| {
            let leg = s.postings.iter().find(|p| p.role == "primary").or_else(|| s.postings.first())?;
            Some((s.id, (leg.account_id, s.chain_seq.unwrap_or(0))))
        })
        .collect();
    let mut out = BTreeMap::new();
    for o in &proj.occurrences {
        if o.kind != "series" {
            continue;
        }
        let Some(&(account, seq)) = primary.get(&o.series_id) else { continue };
        if o.account_id != account {
            continue;
        }
        out.entry((seq, o.occurrence_on.clone())).or_insert(Point {
            series_id: o.series_id,
            seq,
            occurrence_on: o.occurrence_on.clone(),
            value_on: o.value_on.clone(),
            amount_minor: o.amount_minor,
            amended: o.amended,
        });
    }
    out
}

fn point_json(p: &Point) -> Value {
    json!({
        "seq": p.seq,
        "series_id": p.series_id,
        "occurrence_on": p.occurrence_on,
        "value_on": p.value_on,
        "amount_minor": p.amount_minor,
    })
}

// ---------------------------------------------------------------------------------------------
// validation shared by the edit paths
// ---------------------------------------------------------------------------------------------

/// A rule must say something the ledger can keep: no COUNT, no finer than daily, it must parse,
/// and it must produce at least one payment from its start. A stored rule that fails any of these
/// breaks every forecast of the whole book, so it is refused before anything is written.
fn validate_rule(rrule: &str, start: NaiveDate, until: Option<NaiveDate>) -> Result<(), EditError> {
    let up = rrule.to_uppercase();
    if up.contains("COUNT=") {
        return Err(bad("a rule can't say COUNT: set an end date instead (COUNT counts skipped payments too)"));
    }
    if up.contains("FREQ=HOURLY") || up.contains("FREQ=MINUTELY") || up.contains("FREQ=SECONDLY") {
        return Err(bad("a rule repeats at most daily"));
    }
    RRuleCrate
        .expand(rrule, start, None, start, two_years(start))
        .map_err(|e| fail("bad_rule", e.to_string()))?;
    let dates = RRuleCrate
        .expand(rrule, start, until, start, two_years(start))
        .map_err(|e| fail("bad_rule", e.to_string()))?;
    if dates.is_empty() {
        return Err(fail("bad_rule", format!("that rule produces no payments from {}", dmy(start))));
    }
    Ok(())
}

/// Every date in `lo..=hi` a rule produces, ignoring any end date: an end date does not change
/// which slots a rule HAS, only which it still pays, and a payment recorded ahead and then cut off
/// by an end date still claims a slot the schedule check must see.
fn slots(rrule: &str, start: NaiveDate, lo: NaiveDate, hi: NaiveDate) -> Result<BTreeSet<NaiveDate>, recur::RecurError> {
    Ok(RRuleCrate.expand(rrule, start, None, lo, hi)?.into_iter().collect())
}

fn first_slot(hop: &Hop) -> Option<NaiveDate> {
    let start = hop.start();
    RRuleCrate.expand(&hop.rrule, start, hop.until(), start, two_years(start)).ok()?.first().copied()
}

// ---------------------------------------------------------------------------------------------
// series.revise
// ---------------------------------------------------------------------------------------------

fn opt_date(params: &Value, key: &str) -> Result<Option<NaiveDate>, EditError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .and_then(day)
            .map(Some)
            .ok_or_else(|| bad(format!("{key} must be YYYY-MM-DD"))),
    }
}

fn opt_ints(params: &Value, key: &str) -> Result<Option<Vec<i64>>, EditError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_i64().ok_or_else(|| bad(format!("{key} must be a list of whole numbers"))))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(bad(format!("{key} must be a list of whole numbers"))),
    }
}

/// A flag: absent or null is false, a JSON bool is itself, and anything else is refused. Strict on
/// purpose: `dry_run` read as false by mistake would WRITE what the caller asked only to preview.
fn opt_bool(params: &Value, key: &str) -> Result<bool, EditError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad(format!("{key} must be true or false"))),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Terms {
    description: String,
    rrule: String,
    dtstart: NaiveDate,
    until_on: Option<NaiveDate>,
    weekend_rule: String,
    route: Vec<i64>,
    amounts: Vec<i64>,
}

fn terms_json(t: &Terms, accounts: &HashMap<i64, Acct>) -> Value {
    json!({
        "description": t.description,
        "rrule": t.rrule,
        "phrase": recur::describe(&t.rrule, t.dtstart),
        "dtstart": t.dtstart.to_string(),
        "until_on": t.until_on.map(|d| d.to_string()),
        "weekend_rule": t.weekend_rule,
        "route_names": t.route.iter().map(|id| accounts.get(id).map(|a| a.name.clone()).unwrap_or_default()).collect::<Vec<_>>(),
        "amounts": t.amounts,
    })
}

/// Everything a revision or an undo reports once its writes are in place: the forecast before and
/// after, what appears, disappears and changes, and what would fall due now.
struct Outcome {
    impact: Value,
    seam: Value,
    due: Vec<Value>,
}

#[allow(clippy::too_many_arguments)]
fn outcome(
    tx: &Connection,
    before_snap: &Snapshot,
    lineage_ids_before: &HashSet<i64>,
    scenario: Option<i64>,
    as_of: NaiveDate,
    cut: NaiveDate,
    cut_state: &str,
    lineage: i64,
) -> Result<Outcome, EditError> {
    let (before_sub, before) = project_rule(before_snap, lineage_ids_before, scenario, as_of);
    let after_snap = forecast::load::snapshot(tx, as_of)?;
    let after_members: HashSet<i64> = {
        let lm = forecast::lineage_map(&after_snap.series);
        after_snap.series.iter().filter(|s| lm.get(&s.id) == Some(&lineage)).map(|s| s.id).collect()
    };
    let (after_sub, after) = project_rule(&after_snap, &after_members, scenario, as_of);
    let bp = before.as_ref().map(|p| points(&before_sub, p)).unwrap_or_default();
    let ap = after.as_ref().map(|p| points(&after_sub, p)).unwrap_or_default();

    let appears: Vec<&Point> = ap.iter().filter(|(k, _)| !bp.contains_key(*k)).map(|(_, v)| v).collect();
    let disappears: Vec<&Point> = bp.iter().filter(|(k, _)| !ap.contains_key(*k)).map(|(_, v)| v).collect();
    let mut changes: Vec<Value> = Vec::new();
    for (k, a) in &ap {
        if let Some(b) = bp.get(k) {
            if a.value_on != b.value_on || a.amount_minor != b.amount_minor {
                changes.push(json!({
                    "seq": k.0,
                    "occurrence_on": k.1,
                    "before": { "value_on": b.value_on, "amount_minor": b.amount_minor, "series_id": b.series_id },
                    "after": { "value_on": a.value_on, "amount_minor": a.amount_minor, "series_id": a.series_id },
                }));
            }
        }
    }
    let as_of_s = as_of.to_string();
    // Due now: a payment that was not in the forecast and now is, dated today or earlier. Only
    // baseline: a what-if never moves money, so nothing of it can be owed.
    let mut due: Vec<Value> = Vec::new();
    if scenario.is_none() {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for p in &appears {
            if (p.occurrence_on <= as_of_s || p.value_on <= as_of_s) && seen.insert(p.occurrence_on.clone()) {
                due.push(json!({
                    "kind": "due",
                    "occurrence_on": p.occurrence_on,
                    "value_on": p.value_on,
                    "amount_minor": p.amount_minor,
                    "series_ids": [p.series_id],
                    "droppable": false,
                }));
            }
        }
    }

    // The seam: the last payment recorded before the cut, then the forecast's first hop around it,
    // each with the days since the one before -- a doubled month shows up as a gap of a few days.
    let all_ids: Vec<i64> = lineage_ids_before.union(&after_members).copied().collect();
    let claims = claims_for(tx, &all_ids)?;
    let cut_s = cut.to_string();
    let mut seam_rows: Vec<(String, String, Option<i64>, i64, &str)> = Vec::new();
    if let Some(c) = claims.iter().filter(|c| c.occurrence_on < cut_s).max_by(|a, b| {
        (&a.occurrence_on, a.txn_id).cmp(&(&b.occurrence_on, b.txn_id))
    }) {
        seam_rows.push((c.occurrence_on.clone(), c.occurred_on.clone(), None, c.series_id, "recorded"));
    }
    let firsts: Vec<&Point> = ap.values().filter(|p| p.seq == 0).collect();
    for p in firsts.iter().filter(|p| p.occurrence_on < cut_s) {
        seam_rows.push((p.occurrence_on.clone(), p.value_on.clone(), Some(p.amount_minor), p.series_id, "earlier"));
    }
    for p in firsts.iter().filter(|p| p.occurrence_on >= cut_s).take(4) {
        seam_rows.push((p.occurrence_on.clone(), p.value_on.clone(), Some(p.amount_minor), p.series_id, cut_state));
    }
    let mut seam: Vec<Value> = Vec::new();
    let mut previous: Option<NaiveDate> = None;
    for (occ, value, amount, sid, state) in seam_rows {
        let v = day(&value);
        let gap = match (previous, v) {
            (Some(p), Some(v)) => Some((v - p).num_days()),
            _ => None,
        };
        previous = v.or(previous);
        seam.push(json!({
            "occurrence_on": occ, "value_on": value, "amount_minor": amount,
            "series_id": sid, "state": state, "gap_days": gap,
        }));
    }

    let impact = json!({
        "from": as_of_s,
        "to": horizon(as_of).to_string(),
        "appears": appears.iter().map(|p| point_json(p)).collect::<Vec<_>>(),
        "disappears": disappears.iter().map(|p| point_json(p)).collect::<Vec<_>>(),
        "changes": changes,
        "due": due.iter().map(|d| json!({ "occurrence_on": d["occurrence_on"], "value_on": d["value_on"] })).collect::<Vec<_>>(),
        "before_error": before.err(),
        "after_error": after.err(),
    });
    Ok(Outcome { impact, seam: Value::Array(seam), due })
}

pub(crate) fn revise(conn: &mut Connection, params: &Value) -> Result<Value, EditError> {
    // ---- 1. parse: malformed input is an error even on a dry run ----
    let id = params.get("id").and_then(Value::as_i64).ok_or_else(|| bad("id must be a whole number"))?;
    let as_of = opt_date(params, "as_of")?.ok_or_else(|| bad("as_of must be YYYY-MM-DD"))?;
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("whole");
    if mode != "whole" && mode != "from" {
        return Err(bad("mode must be whole or from"));
    }
    let from_on = opt_date(params, "from_on")?;
    if mode == "from" && from_on.is_none() {
        return Err(bad("a change from a date needs from_on"));
    }
    let description_patch = match params.get("description") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| bad("description must be text"))?.trim().to_string()),
    };
    let rrule_patch = match params.get("rrule") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| bad("rrule must be text"))?.trim().to_string()),
    };
    let weekend_patch = match params.get("weekend_rule") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| bad("weekend_rule must be text"))?.to_string()),
    };
    let dtstart_patch = opt_date(params, "dtstart")?;
    // until_on: absent keeps the end, null clears it.
    let until_patch: Option<Option<NaiveDate>> = match params.get("until_on") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => Some(Some(v.as_str().and_then(day).ok_or_else(|| bad("until_on must be YYYY-MM-DD"))?)),
    };
    let route_patch = opt_ints(params, "route")?;
    let amounts_patch = opt_ints(params, "amounts")?;
    let follow = match params.get("amends").and_then(Value::as_str) {
        None | Some("keep") => false,
        Some("follow") => true,
        Some(_) => return Err(bad("amends must be keep or follow")),
    };
    let drop_overrides = opt_bool(params, "drop_overrides")?;
    let acknowledge_due = opt_bool(params, "acknowledge_due")?;
    let dry_run = opt_bool(params, "dry_run")?;

    // ---- 2. load the rule ----
    let tx = conn.transaction()?;
    let hops = load_hops(&tx)?;
    let accounts = load_accounts(&tx)?;
    let scenarios = load_scenarios(&tx)?;
    let parts = parts_of(&hops);
    let lineage = parts
        .iter()
        .find(|p| p.ids().contains(&id))
        .map(Part::lineage)
        .ok_or_else(|| fail("not_found", format!("no such recurring payment: {id}")))?;
    let rule_parts = rules_of(parts).remove(&lineage).unwrap_or_default();
    let l = rule_parts.last().cloned().ok_or_else(|| fail("not_found", format!("no such recurring payment: {id}")))?;
    let q = if rule_parts.len() >= 2 { Some(rule_parts[rule_parts.len() - 2].clone()) } else { None };
    let lineage_ids: Vec<i64> = rule_parts.iter().flat_map(Part::ids).collect();
    let desc = l.first().description.clone();

    // ---- 3-4. which part, and which kind of edit ----
    if !l.ids().contains(&id) {
        return Err(fail(
            "not_latest",
            format!("{desc} changed on {}: edit its newest part, or undo that change first", dmy(l.first().start())),
        ));
    }
    if l.is_cancel() {
        let target = l.first().supersedes_id.and_then(|t| hops.iter().find(|h| h.id == t)).map(|h| h.description.clone()).unwrap_or_default();
        let scenario = l.first().scenario_id.and_then(|s| scenarios.get(&s).cloned()).unwrap_or_default();
        return Err(fail(
            "not_editable",
            format!("this row only cancels {target} inside the what-if “{scenario}”; it has nothing to edit"),
        ));
    }
    let what_if = l.first().scenario_id;
    if mode == "from" && what_if.is_some() {
        return Err(fail("not_editable", "a what-if has no recorded history to keep, so it is changed as a whole"));
    }

    let shape = shape_of(&l);
    let before = Terms {
        description: desc.clone(),
        rrule: l.first().rrule.clone(),
        dtstart: l.first().start(),
        until_on: l.first().until(),
        weekend_rule: l.first().weekend_rule.clone(),
        route: shape.route.clone(),
        amounts: shape.magnitudes.clone(),
    };

    // ---- 5. undated fields ----
    let description = match description_patch {
        Some(d) if d.is_empty() => return Err(bad("description must not be empty")),
        Some(d) => d,
        None => before.description.clone(),
    };
    let weekend_rule = match weekend_patch {
        Some(w) if !WEEKEND_RULES.contains(&w.as_str()) => {
            return Err(bad("weekend_rule must be none, before, after or modified_after"))
        }
        Some(w) => w,
        None => before.weekend_rule.clone(),
    };
    let until_on = until_patch.unwrap_or(before.until_on);
    let rrule = rrule_patch.unwrap_or_else(|| before.rrule.clone());
    let rrule_changed = norm_rule(&rrule) != norm_rule(&before.rrule);
    let rrule = if rrule_changed { rrule } else { before.rrule.clone() };

    // ---- 7. start ----
    if mode == "whole" {
        if let Some(ds) = dtstart_patch {
            if ds != before.dtstart && q.is_some() {
                return Err(fail(
                    "overlaps_part",
                    format!("{desc} changed on {}: its start can't move — undo that change and make it again", dmy(before.dtstart)),
                ));
            }
        }
    }

    // ---- 8. money: route and amounts ----
    if (route_patch.is_some() || amounts_patch.is_some()) && !shape.two_leg {
        let names: Vec<String> = l
            .hops
            .iter()
            .flat_map(|h| h.legs.iter())
            .map(|leg| accounts.get(&leg.account_id).map(|a| a.name.clone()).unwrap_or_default())
            .collect();
        let n = l.hops.iter().map(|h| h.legs.len()).sum::<usize>();
        return Err(fail(
            "custom_template",
            format!(
                "{desc} has {n} legs of its own ({}): its amounts and accounts can't be edited here, only its name, schedule, weekend rule and end",
                names.join(", ")
            ),
        ));
    }
    let route = route_patch.unwrap_or_else(|| before.route.clone());
    let amounts = amounts_patch.unwrap_or_else(|| before.amounts.clone());
    let route_changed = route != before.route;
    let amounts_changed = amounts != before.amounts;
    if route_changed || amounts_changed {
        let hops_n = l.hops.len();
        if route.len() != hops_n + 1 {
            return Err(fail(
                "bad_chain",
                format!("{desc} has {hops_n} leg{}: stops can't be added or removed here — end it and create a new one", if hops_n == 1 { "" } else { "s" }),
            ));
        }
        if amounts.len() != hops_n {
            return Err(bad(format!("amounts needs one amount per leg: {desc} has {hops_n}")));
        }
        if let Some(a) = amounts.iter().find(|&&a| a <= 0) {
            return Err(bad(format!("an amount is always positive, and {a} is not one")));
        }
        if route.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(bad("a payment cannot move money from an account to itself"));
        }
        for (i, acc) in route.iter().enumerate() {
            let a = accounts.get(acc).ok_or_else(|| fail("no_such_account", format!("no such account: {acc}")))?;
            let already = before.route.contains(acc);
            if a.system && !already {
                return Err(fail("system_account", format!("{} is a system account -- only the core may post to it", a.name)));
            }
            if a.closed && !already {
                return Err(fail("closed_account", format!("{} is closed: reopen it before moving {desc} onto it", a.name)));
            }
            if a.currency != shape.currency {
                return Err(fail(
                    "currency_change",
                    format!(
                        "{desc} is in {} and {} holds {}: a recurring payment can't change currency, because its amounts and adjustments are stored in {} — end it and create a new one",
                        shape.currency, a.name, a.currency, shape.currency
                    ),
                ));
            }
            if i > 0 && i < route.len() - 1 && a.kind != "asset" && a.kind != "liability" {
                return Err(fail(
                    "bad_chain",
                    format!("money can only pass through an asset or liability account: {} is an {} account", a.name, a.kind),
                ));
            }
        }
    }

    // ---- 9. from a date: split, or in place when nothing of the rule falls before it ----
    let mut split = false;
    let mut first_new: Option<NaiveDate> = None;
    let dated = rrule_changed || route_changed || amounts_changed;
    let dtstart = if mode == "from" {
        let from = from_on.expect("checked above");
        // The date only matters to a change that depends on one. A new name, end or weekend rule
        // applies to the current terms as they stand, whatever date the form happens to hold.
        if dated {
            if let Some(q) = &q {
                if q.first().until().is_some_and(|u| from <= u) {
                    return Err(fail(
                        "overlaps_part",
                        format!(
                            "{} belongs to the terms before the change on {}: change it from {} or later, or undo that change",
                            dmy(from), dmy(before.dtstart), dmy(before.dtstart)
                        ),
                    ));
                }
            }
            if let Some(u) = before.until_on {
                if from > u {
                    return Err(fail(
                        "already_ended",
                        format!("{desc} ends on {}, before a change from {} would apply: move its end first", dmy(u), dmy(from)),
                    ));
                }
            }
        }
        let has_slot_before = from > before.dtstart
            && match RRuleCrate.expand(&before.rrule, before.dtstart, before.until_on, before.dtstart, from - Duration::days(1)) {
                Ok(d) => !d.is_empty(),
                Err(_) => true,
            };
        // The new part keeps the rule's rhythm: an unchanged fortnightly or yearly rule is anchored
        // at its own start, so the first payment of the new part is the first slot it already had
        // on or after the date, not a new cycle begun on that date.
        let anchor = if rrule_changed { from } else { before.dtstart };
        let find_d = || -> Result<NaiveDate, EditError> {
            RRuleCrate
                .expand(&rrule, anchor, until_on, from, two_years(from))
                .map_err(|e| fail("bad_rule", e.to_string()))?
                .first()
                .copied()
                .ok_or_else(|| fail("bad_rule", format!("that rule produces no payments from {}", dmy(from))))
        };
        if !dated {
            before.dtstart
        } else if !has_slot_before {
            if rrule_changed { find_d()? } else { before.dtstart }
        } else {
            split = true;
            let d = find_d()?;
            first_new = Some(d);
            d
        }
    } else if let (Some(q), true) = (&q, rrule_changed) {
        // The newest part of a changed rule starts where its first payment fell, which can be well
        // after the day the earlier part ended. A new schedule written from that start would leave
        // its payments in between belonging to neither part, so it starts from the day after the
        // earlier part ends instead: the first date the new schedule pays from there.
        let from = q.first().until().map_or(before.dtstart, |u| u + Duration::days(1));
        RRuleCrate
            .expand(&rrule, from, until_on, from, two_years(from))
            .map_err(|e| fail("bad_rule", e.to_string()))?
            .first()
            .copied()
            .ok_or_else(|| match until_on {
                Some(u) if u < two_years(from) => {
                    fail("bad_rule", format!("that rule pays nothing between {} and the end on {}", dmy(from), dmy(u)))
                }
                _ => fail("bad_rule", format!("that rule produces no payments from {}", dmy(from))),
            })?
    } else {
        dtstart_patch.unwrap_or(before.dtstart)
    };
    let start_changed = dtstart != before.dtstart;
    if (rrule_changed || start_changed) && !split {
        validate_rule(&rrule, dtstart, until_on)?;
    }
    if split {
        validate_rule(&rrule, dtstart, until_on)?;
    }

    // ---- 10. the end ----
    if let Some(u) = until_on {
        if u < dtstart {
            return Err(bad(format!("it would end on {}, before it starts on {}", dmy(u), dmy(dtstart))));
        }
    }

    // ---- 11. nothing to do ----
    let after = Terms {
        description: description.clone(),
        rrule: rrule.clone(),
        dtstart,
        until_on,
        weekend_rule: weekend_rule.clone(),
        route: route.clone(),
        amounts: amounts.clone(),
    };
    let mut changed: Vec<&str> = Vec::new();
    if after.description != before.description { changed.push("description"); }
    if rrule_changed { changed.push("rrule"); }
    if after.dtstart != before.dtstart { changed.push("dtstart"); }
    if after.until_on != before.until_on { changed.push("until_on"); }
    if after.weekend_rule != before.weekend_rule { changed.push("weekend_rule"); }
    if route_changed { changed.push("route"); }
    if amounts_changed { changed.push("amounts"); }
    if changed.is_empty() {
        return Ok(json!({
            "ok": true, "dry_run": dry_run, "written": false, "mode": "unchanged", "lineage_id": lineage,
            "changed": [], "applied_to": [], "ended": null, "created": null, "anchor": null,
            "blockers": [], "refusal": null, "suggest_from": null, "carried": [], "left_behind": [],
            "dropped": [], "left_on_old_dates": [], "dormant": [],
            "amends": { "follow": [], "keep": [], "applied": false },
            "before": terms_json(&before, &accounts), "after": terms_json(&after, &accounts),
            "impact": null, "seam": [],
        }));
    }

    // ---- execution ----
    let lookback_from = as_of - Duration::days(LOOKBACK_DAYS);
    let l_ids = l.ids();
    let claims = claims_for(&tx, &l_ids)?;
    let adjustments = adjustments_for(&tx, &l_ids)?;
    let before_snap = forecast::load::snapshot(&tx, as_of)?;
    let lineage_set: HashSet<i64> = lineage_ids.iter().copied().collect();
    let seq_of: HashMap<i64, usize> = l.hops.iter().enumerate().map(|(k, h)| (h.id, k)).collect();

    let mut blockers: Vec<Value> = Vec::new();
    let mut recorded_blockers: Vec<(NaiveDate, String)> = Vec::new(); // (slot, paid on)
    let mut suggest_from: Option<NaiveDate> = None;
    let mut dropped: Vec<Value> = Vec::new();
    let mut dropped_dates: BTreeSet<String> = BTreeSet::new();
    let mut left_on_old_dates: Vec<Value> = Vec::new();
    let mut carried: Vec<Value> = Vec::new();
    let mut left_behind: Vec<Value> = Vec::new();
    let mut applied_to: BTreeSet<i64> = BTreeSet::new();
    let mut ended: Value = Value::Null;
    let mut created: Value = Value::Null;
    let mut anchor: Value = Value::Null;
    let mut target_ids: Vec<i64> = l_ids.clone();

    // Claims grouped per slot across hops.
    let mut claims_by_date: BTreeMap<String, Vec<&Claim>> = BTreeMap::new();
    for c in &claims {
        claims_by_date.entry(c.occurrence_on.clone()).or_default().push(c);
    }
    let mut adj_by_date: BTreeMap<String, Vec<&Adjustment>> = BTreeMap::new();
    for a in &adjustments {
        adj_by_date.entry(a.occurrence_on.clone()).or_default().push(a);
    }
    let amounts_of = |rows: &[&Adjustment]| -> Vec<Option<i64>> {
        let mut out = vec![None; l.hops.len()];
        for r in rows {
            if let Some(&k) = seq_of.get(&r.series_id) {
                out[k] = r.amount_minor.map(i64::abs);
            }
        }
        out
    };
    let recorded_blocker = |date: &str, rows: &[&Claim]| -> Value {
        json!({
            "kind": "recorded",
            "occurrence_on": date,
            "value_on": rows.first().map(|c| c.occurred_on.clone()),
            "occurred_on": rows.first().map(|c| c.occurred_on.clone()),
            "series_ids": rows.iter().map(|c| c.series_id).collect::<Vec<_>>(),
            "txn_ids": rows.iter().map(|c| c.txn_id).collect::<Vec<_>>(),
            "droppable": false,
        })
    };

    if !split {
        // ---- in place ----
        if rrule_changed || start_changed {
            let keys: BTreeSet<NaiveDate> = claims
                .iter()
                .map(|c| c.occurrence_on.as_str())
                .chain(adjustments.iter().map(|a| a.occurrence_on.as_str()))
                .filter_map(day)
                .collect();
            if let (Some(&lo), Some(&hi)) = (keys.first(), keys.last()) {
                let old = slots(&before.rrule, before.dtstart, lo, hi).ok();
                let new = slots(&rrule, dtstart, lo, hi).map_err(|e| fail("bad_rule", e.to_string()))?;
                let counts = |d: NaiveDate| old.as_ref().is_none_or(|o| o.contains(&d)) && !new.contains(&d);
                for (date, rows) in &claims_by_date {
                    if day(date).is_some_and(&counts) {
                        recorded_blockers.push((day(date).unwrap(), rows[0].occurred_on.clone()));
                        blockers.push(recorded_blocker(date, rows));
                    }
                }
                for (date, rows) in &adj_by_date {
                    let Some(d) = day(date) else { continue };
                    if !counts(d) {
                        continue;
                    }
                    if d >= lookback_from {
                        blockers.push(json!({
                            "kind": "override", "occurrence_on": date, "action": rows[0].action,
                            "word": action_word(rows[0]), "moved_to": rows[0].moved_to,
                            "amounts": amounts_of(rows), "series_ids": rows.iter().map(|r| r.series_id).collect::<Vec<_>>(),
                            "droppable": true,
                        }));
                        tx.execute(
                            &format!("DELETE FROM series_override WHERE occurrence_on = ?1 AND series_id IN ({})", id_list(&l_ids)),
                            params![date],
                        )?;
                        dropped.push(json!({
                            "occurrence_on": date, "action": rows[0].action, "word": action_word(rows[0]),
                            "moved_to": rows[0].moved_to, "amounts": amounts_of(rows),
                        }));
                        dropped_dates.insert(date.clone());
                    } else {
                        left_on_old_dates.push(json!({ "occurrence_on": date, "action": rows[0].action, "word": action_word(rows[0]) }));
                    }
                }
            }
            if !recorded_blockers.is_empty() {
                let latest = claims.iter().filter_map(|c| day(&c.occurrence_on)).max();
                let first = first_slot(l.first()).unwrap_or(before.dtstart);
                suggest_from = latest.map(|d| (d + Duration::days(1)).max(first + Duration::days(1)));
            }
        }
        if after.description != before.description {
            tx.execute(&format!("UPDATE series SET description = ?1 WHERE id IN ({})", id_list(&lineage_ids)), params![description])?;
            applied_to.extend(lineage_ids.iter().copied());
        }
        tx.execute(
            &format!("UPDATE series SET rrule = ?1, dtstart = ?2, until_on = ?3, weekend_rule = ?4 WHERE id IN ({})", id_list(&l_ids)),
            params![rrule, dtstart.to_string(), until_on.map(|d| d.to_string()), weekend_rule],
        )?;
        applied_to.extend(l_ids.iter().copied());
        if route_changed || amounts_changed {
            write_money(&tx, &l, &shape, &route, &amounts, None)?;
        }
    } else {
        // ---- split ----
        let from = from_on.expect("checked above");
        let d = first_new.expect("set with split");
        let from_s = from.to_string();
        for (date, rows) in claims_by_date.range(from_s.clone()..) {
            recorded_blockers.push((day(date).unwrap_or(from), rows[0].occurred_on.clone()));
            blockers.push(recorded_blocker(date, rows));
        }
        if let Some((latest, _)) = recorded_blockers.last() {
            suggest_from = Some(*latest + Duration::days(1));
        }
        let old_until = from - Duration::days(1);
        if after.description != before.description {
            tx.execute(&format!("UPDATE series SET description = ?1 WHERE id IN ({})", id_list(&lineage_ids)), params![description])?;
            applied_to.extend(lineage_ids.iter().copied());
        }
        tx.execute(
            &format!("UPDATE series SET until_on = ?1 WHERE id IN ({})", id_list(&l_ids)),
            params![old_until.to_string()],
        )?;
        applied_to.extend(l_ids.iter().copied());
        ended = json!({ "ids": l_ids, "until_on": old_until.to_string() });

        let mut new_ids: Vec<i64> = Vec::with_capacity(l.hops.len());
        let mut chain_head: Option<i64> = None;
        let mut head_ids: Vec<i64> = Vec::new();
        for (k, h) in l.hops.iter().enumerate() {
            let head = h.head_id.unwrap_or(h.id);
            tx.execute(
                "INSERT INTO series(head_id, description, rrule, dtstart, until_on, weekend_rule, holiday_cal,
                                    match_window_before, match_window_after, match_amount_minor,
                                    match_amount_bp, match_auto, scenario_id, supersedes_id, chain_id, chain_seq)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL,NULL,?13,?14)",
                params![
                    head, description, rrule, d.to_string(), until_on.map(|u| u.to_string()), weekend_rule,
                    h.holiday_cal, h.match_window_before, h.match_window_after, h.match_amount_minor,
                    h.match_amount_bp, h.match_auto, chain_head, chain_head.map(|_| k as i64)
                ],
            )?;
            let new_id = tx.last_insert_rowid();
            if l.is_chain() && chain_head.is_none() {
                tx.execute("UPDATE series SET chain_id = ?1, chain_seq = 0 WHERE id = ?1", [new_id])?;
                chain_head = Some(new_id);
            }
            if !(route_changed || amounts_changed) {
                for leg in &h.legs {
                    tx.execute(
                        "INSERT INTO series_posting(series_id, account_id, currency, amount_minor, role)
                         VALUES(?1,?2,?3,?4,?5)",
                        params![new_id, leg.account_id, leg.currency, leg.amount_minor, leg.role],
                    )?;
                }
            }
            new_ids.push(new_id);
            head_ids.push(head);
        }
        if route_changed || amounts_changed {
            write_money(&tx, &l, &shape, &route, &amounts, Some(&new_ids))?;
        }
        applied_to.extend(new_ids.iter().copied());

        // Adjustments on or after the date travel to the new part when the new rule still has a
        // payment that day; otherwise they stay with the earlier part, which now ends before them
        // and so no longer applies them, and come back only if the change is undone.
        let later: Vec<(&String, &Vec<&Adjustment>)> = adj_by_date.range(from_s.clone()..).collect();
        if let (Some(lo), Some(hi)) = (later.first().and_then(|x| day(x.0)), later.last().and_then(|x| day(x.0))) {
            let new_slots = slots(&rrule, d, lo, hi).map_err(|e| fail("bad_rule", e.to_string()))?;
            for (date, rows) in later {
                let carries = day(date).is_some_and(|x| new_slots.contains(&x));
                if carries {
                    for r in rows {
                        let k = seq_of[&r.series_id];
                        tx.execute(
                            "UPDATE series_override SET series_id = ?1 WHERE series_id = ?2 AND occurrence_on = ?3",
                            params![new_ids[k], r.series_id, date],
                        )?;
                    }
                    carried.push(json!({ "occurrence_on": date, "action": rows[0].action, "word": action_word(rows[0]), "moved_to": rows[0].moved_to }));
                } else {
                    left_behind.push(json!({
                        "occurrence_on": date, "action": rows[0].action, "word": action_word(rows[0]),
                        "reason": "the new rule has no payment that day",
                    }));
                }
            }
        }
        created = json!({ "ids": new_ids, "chain_id": chain_head, "dtstart": d.to_string(), "head_ids": head_ids });
        anchor = json!({ "rrule_unchanged": !rrule_changed, "from_on": from_s, "first_slot": d.to_string() });
        target_ids = new_ids;
    }

    // ---- amends follow the new price ----
    let mut amends_follow: Vec<Value> = Vec::new();
    let mut amends_keep: Vec<Value> = Vec::new();
    if amounts_changed {
        let rows = adjustments_for(&tx, &target_ids)?;
        let target_seq: HashMap<i64, usize> = target_ids.iter().enumerate().map(|(k, id)| (*id, k)).collect();
        let mut waves: BTreeMap<String, Vec<Option<Adjustment>>> = BTreeMap::new();
        for r in rows {
            if day(&r.occurrence_on).is_none_or(|d| d < lookback_from) {
                continue;
            }
            let k = target_seq[&r.series_id];
            let on = r.occurrence_on.clone();
            waves.entry(on).or_insert_with(|| vec![None; target_ids.len()])[k] = Some(r);
        }
        for (date, wave) in &waves {
            let any_amount = wave.iter().flatten().any(|r| r.amount_minor.is_some());
            if !any_amount {
                continue;
            }
            // Candidates: every hop carries exactly its OLD template magnitude. That is an amount the
            // editor stored when only the date moved, not a price of its own.
            let candidate = wave.iter().enumerate().all(|(k, r)| {
                r.as_ref().is_some_and(|r| r.amount_minor.is_some_and(|a| a.abs() == before.amounts[k]))
            });
            if candidate {
                let removed = wave
                    .iter()
                    .flatten()
                    .all(|r| r.action == "amend" && r.moved_to.is_none() && r.description.is_none() && r.note.is_none());
                if follow {
                    for r in wave.iter().flatten() {
                        if r.action == "amend" && r.moved_to.is_none() && r.description.is_none() && r.note.is_none() {
                            tx.execute("DELETE FROM series_override WHERE series_id = ?1 AND occurrence_on = ?2", params![r.series_id, date])?;
                        } else {
                            tx.execute("UPDATE series_override SET amount_minor = NULL WHERE series_id = ?1 AND occurrence_on = ?2", params![r.series_id, date])?;
                        }
                    }
                }
                amends_follow.push(json!({ "occurrence_on": date, "removed": removed }));
            } else {
                amends_keep.push(json!({
                    "occurrence_on": date,
                    "amounts": wave.iter().map(|r| r.as_ref().and_then(|r| r.amount_minor.map(i64::abs))).collect::<Vec<_>>(),
                }));
            }
        }
    }

    // ---- dormant: what now falls after the end ----
    let mut dormant: Vec<Value> = Vec::new();
    if let Some(u) = until_on {
        if after.until_on != before.until_on {
            let u_s = u.to_string();
            let old_end = before.until_on.map(|d| d.to_string());
            let mut seen: BTreeSet<(String, &str)> = BTreeSet::new();
            let was_live = |date: &String| old_end.as_ref().is_none_or(|e| date <= e);
            for c in &claims {
                if c.occurrence_on > u_s && was_live(&c.occurrence_on) && seen.insert((c.occurrence_on.clone(), "recorded")) {
                    dormant.push(json!({ "occurrence_on": c.occurrence_on, "kind": "recorded" }));
                }
            }
            for a in &adjustments {
                if a.occurrence_on > u_s && was_live(&a.occurrence_on) && !dropped_dates.contains(&a.occurrence_on)
                    && seen.insert((a.occurrence_on.clone(), "adjustment"))
                {
                    dormant.push(json!({ "occurrence_on": a.occurrence_on, "kind": "adjustment", "word": action_word(a) }));
                }
            }
        }
    }

    // ---- after: impact, seam, due ----
    let cut = first_new.unwrap_or(as_of);
    let out = outcome(&tx, &before_snap, &lineage_set, what_if, as_of, cut, if split { "new" } else { "current" }, lineage)?;
    blockers.extend(out.due.iter().cloned());

    // ---- refusal ----
    let refusal: Option<EditError> = if !recorded_blockers.is_empty() {
        let dates: Vec<String> = recorded_blockers.iter().map(|(d, _)| dmy(*d)).collect();
        let suggest = suggest_from.map(dmy).unwrap_or_default();
        if split {
            let paid: Vec<String> = recorded_blockers.iter().map(|(d, paid)| format!("{} (paid {})", dmy(*d), dmy_iso(paid))).collect();
            let what = if paid.len() == 1 { "a payment recorded for" } else { "payments recorded for" };
            let pronoun = if paid.len() == 1 { "that payment" } else { "those payments" };
            Some(fail(
                "recorded_after",
                format!("{desc} has {what} {}: change it from {suggest} or later, or delete {pronoun} in Payments", list_words(&paid)),
            ))
        } else {
            let verb = if dates.len() == 1 { "is" } else { "are" };
            Some(fail(
                "slots_detached",
                format!("{desc}'s schedule can't change for the whole rule: {} {verb} recorded as paid under it — change it from {suggest} instead", list_words(&dates)),
            ))
        }
    } else if !dropped.is_empty() && !drop_overrides {
        let items: Vec<String> = dropped
            .iter()
            .map(|d| format!("{} ({})", dmy_iso(d["occurrence_on"].as_str().unwrap_or("")), d["word"].as_str().unwrap_or("")))
            .collect();
        let those = if items.len() == 1 { "that adjustment" } else { "those adjustments" };
        Some(fail(
            "would_drop",
            format!("the new schedule has no payment on {}: confirm to remove {those}", list_words(&items)),
        ))
    } else if !out.due.is_empty() && !acknowledge_due {
        let items: Vec<String> = out
            .due
            .iter()
            .map(|d| format!("{} would fall due now (money moves {})", dmy_iso(d["occurrence_on"].as_str().unwrap_or("")), dmy_iso(d["value_on"].as_str().unwrap_or(""))))
            .collect();
        Some(fail(
            "would_fall_due",
            format!("{} — if it's already paid, record it first; confirm to save anyway", list_words(&items)),
        ))
    } else {
        None
    };

    let result = json!({
        "ok": refusal.is_none(),
        "dry_run": dry_run,
        "written": !dry_run && refusal.is_none(),
        "mode": if split { "split" } else { "in_place" },
        "lineage_id": lineage,
        "changed": changed,
        "applied_to": applied_to.into_iter().collect::<Vec<_>>(),
        "ended": ended,
        "created": created,
        "anchor": anchor,
        "blockers": blockers,
        "refusal": refusal.as_ref().map(|r| json!({ "code": r.code, "message": r.message })),
        "suggest_from": suggest_from.map(|d| d.to_string()),
        "carried": carried,
        "left_behind": left_behind,
        "dropped": dropped,
        "left_on_old_dates": left_on_old_dates,
        "dormant": dormant,
        "amends": { "follow": amends_follow, "keep": amends_keep, "applied": follow && amounts_changed },
        "before": terms_json(&before, &accounts),
        "after": terms_json(&after, &accounts),
        "impact": out.impact,
        "seam": out.seam,
    });

    if dry_run {
        drop(tx); // rolls back
        return Ok(result);
    }
    if let Some(r) = refusal {
        return Err(r);
    }
    tx.commit()?;
    Ok(result)
}

/// Write a part's legs from a route and one magnitude per hop, keeping each hop's DIRECTION:
/// the primary leg stays on the side money leaves when it did before (and the side it arrives
/// when the template was written the other way round), so moving a rent to another account never
/// turns it into income. `new_ids` names freshly inserted hops that have no legs yet; without it
/// the part's own legs are updated in place.
fn write_money(
    tx: &Connection,
    part: &Part,
    shape: &Shape,
    route: &[i64],
    amounts: &[i64],
    new_ids: Option<&[i64]>,
) -> Result<(), EditError> {
    for (k, h) in part.hops.iter().enumerate() {
        let neg = shape.negative[k];
        let m = amounts[k];
        let (p_acc, b_acc) = if neg { (route[k], route[k + 1]) } else { (route[k + 1], route[k]) };
        let (p_amt, b_amt) = if neg { (-m, m) } else { (m, -m) };
        match new_ids {
            Some(ids) => {
                for (acc, amt, role) in [(p_acc, p_amt, "primary"), (b_acc, b_amt, "balancing")] {
                    tx.execute(
                        "INSERT INTO series_posting(series_id, account_id, currency, amount_minor, role) VALUES(?1,?2,?3,?4,?5)",
                        params![ids[k], acc, shape.currency, amt, role],
                    )?;
                }
            }
            None => {
                for (acc, amt, role) in [(p_acc, p_amt, "primary"), (b_acc, b_amt, "balancing")] {
                    let leg = h.legs.iter().find(|l| l.role == role).expect("two-leg shape has both roles");
                    tx.execute(
                        "UPDATE series_posting SET account_id = ?1, currency = ?2, amount_minor = ?3 WHERE id = ?4",
                        params![acc, shape.currency, amt, leg.id],
                    )?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// series.undo_change
// ---------------------------------------------------------------------------------------------

pub(crate) fn undo_change(conn: &mut Connection, params: &Value) -> Result<Value, EditError> {
    let id = params.get("id").and_then(Value::as_i64).ok_or_else(|| bad("id must be a whole number"))?;
    let as_of = opt_date(params, "as_of")?.ok_or_else(|| bad("as_of must be YYYY-MM-DD"))?;
    let acknowledge_due = opt_bool(params, "acknowledge_due")?;
    let dry_run = opt_bool(params, "dry_run")?;

    let tx = conn.transaction()?;
    let hops = load_hops(&tx)?;
    let parts = parts_of(&hops);
    let lineage = parts
        .iter()
        .find(|p| p.ids().contains(&id))
        .map(Part::lineage)
        .ok_or_else(|| fail("not_found", format!("no such recurring payment: {id}")))?;
    let rule_parts = rules_of(parts).remove(&lineage).unwrap_or_default();
    let l = rule_parts.last().cloned().ok_or_else(|| fail("not_found", format!("no such recurring payment: {id}")))?;
    let desc = l.first().description.clone();
    if !l.ids().contains(&id) {
        return Err(fail(
            "not_latest",
            format!("{desc} changed on {}: edit its newest part, or undo that change first", dmy(l.first().start())),
        ));
    }
    if rule_parts.len() < 2 {
        return Err(fail("not_a_change", format!("{desc} was never changed from a date — there is nothing to undo")));
    }
    let q = rule_parts[rule_parts.len() - 2].clone();
    let l_ids = l.ids();
    let q_ids = q.ids();
    let claims = claims_for(&tx, &l_ids)?;
    if !claims.is_empty() {
        let mut seen = BTreeSet::new();
        let items: Vec<String> = claims
            .iter()
            .filter(|c| seen.insert(c.occurrence_on.clone()))
            .map(|c| format!("{} (paid {})", dmy_iso(&c.occurrence_on), dmy_iso(&c.occurred_on)))
            .collect();
        let what = if items.len() == 1 { "a payment is" } else { "payments are" };
        let pronoun = if items.len() == 1 { "it" } else { "them" };
        return Err(fail(
            "change_has_payments",
            format!(
                "{what} recorded under the change from {}: {} — delete {pronoun} in Payments first, or keep the change",
                dmy(l.first().start()),
                list_words(&items)
            ),
        ));
    }

    let lineage_ids: HashSet<i64> = rule_parts.iter().flat_map(Part::ids).collect();
    let before_snap = forecast::load::snapshot(&tx, as_of)?;

    // 1. Adjustments on the change go back to the earlier part when its rule has that slot.
    let adjustments = adjustments_for(&tx, &l_ids)?;
    let q_adjusted: HashSet<(i64, String)> = adjustments_for(&tx, &q_ids)?
        .into_iter()
        .map(|a| (a.series_id, a.occurrence_on))
        .collect();
    let mut by_date: BTreeMap<String, Vec<&Adjustment>> = BTreeMap::new();
    for a in &adjustments {
        by_date.entry(a.occurrence_on.clone()).or_default().push(a);
    }
    let mut carried_back: Vec<Value> = Vec::new();
    let mut lost: Vec<Value> = Vec::new();
    if let (Some(lo), Some(hi)) = (by_date.keys().next().and_then(|d| day(d)), by_date.keys().last().and_then(|d| day(d))) {
        let q_slots = slots(&q.first().rrule, q.first().start(), lo, hi).unwrap_or_default();
        let seq_of: HashMap<i64, usize> = l.hops.iter().enumerate().map(|(k, h)| (h.id, k)).collect();
        for (date, rows) in &by_date {
            let fits = day(date).is_some_and(|d| q_slots.contains(&d))
                && rows.iter().all(|r| {
                    let k = seq_of[&r.series_id];
                    q_ids.get(k).is_some_and(|qk| !q_adjusted.contains(&(*qk, date.clone())))
                });
            if fits {
                for r in rows {
                    let k = seq_of[&r.series_id];
                    tx.execute(
                        "UPDATE series_override SET series_id = ?1 WHERE series_id = ?2 AND occurrence_on = ?3",
                        params![q_ids[k], r.series_id, date],
                    )?;
                }
                carried_back.push(json!({ "occurrence_on": date, "action": rows[0].action, "word": action_word(rows[0]), "moved_to": rows[0].moved_to }));
            } else {
                lost.push(json!({ "occurrence_on": date, "action": rows[0].action, "word": action_word(rows[0]), "moved_to": rows[0].moved_to }));
            }
        }
    }

    // 2. A what-if cancel made against the change now names the earlier part.
    let mut repointed: Vec<i64> = Vec::new();
    {
        let mut st = tx.prepare(&format!("SELECT id FROM series WHERE supersedes_id IN ({})", id_list(&l_ids)))?;
        for r in st.query_map([], |r| r.get::<_, i64>(0))? {
            repointed.push(r?);
        }
    }
    if !repointed.is_empty() {
        tx.execute(
            &format!("UPDATE series SET supersedes_id = ?1 WHERE supersedes_id IN ({})", id_list(&l_ids)),
            params![q.first().id],
        )?;
    }

    // 3-4. The earlier part runs on as far as the change would have, and the change goes.
    let l_until = l.first().until_on.clone();
    tx.execute(&format!("UPDATE series SET until_on = ?1 WHERE id IN ({})", id_list(&q_ids)), params![l_until])?;
    tx.execute(&format!("DELETE FROM series WHERE id IN ({})", id_list(&l_ids)), [])?;

    let out = outcome(&tx, &before_snap, &lineage_ids, q.first().scenario_id, as_of, as_of, "current", lineage)?;
    let refusal = if !out.due.is_empty() && !acknowledge_due {
        let items: Vec<String> = out
            .due
            .iter()
            .map(|d| format!("{} would fall due now (money moves {})", dmy_iso(d["occurrence_on"].as_str().unwrap_or("")), dmy_iso(d["value_on"].as_str().unwrap_or(""))))
            .collect();
        Some(fail("would_fall_due", format!("{} — if it's already paid, record it first; confirm to save anyway", list_words(&items))))
    } else {
        None
    };
    let result = json!({
        "ok": refusal.is_none(),
        "dry_run": dry_run,
        "written": !dry_run && refusal.is_none(),
        "mode": "undone",
        "lineage_id": lineage,
        "restored": { "ids": q_ids, "until_on": l_until },
        "removed": l_ids,
        "carried_back": carried_back,
        "lost": lost,
        "repointed_cancels": repointed,
        "blockers": out.due,
        "refusal": refusal.as_ref().map(|r| json!({ "code": r.code, "message": r.message })),
        "impact": out.impact,
        "seam": out.seam,
    });
    if dry_run {
        drop(tx);
        return Ok(result);
    }
    if let Some(r) = refusal {
        return Err(r);
    }
    tx.commit()?;
    Ok(result)
}

// ---------------------------------------------------------------------------------------------
// series.end and series.rename, rule-aware
// ---------------------------------------------------------------------------------------------

/// A rule's newest part, the part before it (if any), and every series id of the rule.
type Newest = (Part, Option<Part>, Vec<i64>);

/// The newest part of the rule `id` belongs to, and the part before it. None when there is no
/// such series.
fn newest(conn: &Connection, id: i64) -> Result<Option<Newest>, EditError> {
    let hops = load_hops(conn)?;
    let parts = parts_of(&hops);
    let Some(lineage) = parts.iter().find(|p| p.ids().contains(&id)).map(Part::lineage) else {
        return Ok(None);
    };
    let rule_parts = rules_of(parts).remove(&lineage).unwrap_or_default();
    let all: Vec<i64> = rule_parts.iter().flat_map(Part::ids).collect();
    let n = rule_parts.len();
    let q = if n >= 2 { Some(rule_parts[n - 2].clone()) } else { None };
    Ok(rule_parts.last().cloned().map(|l| (l, q, all)))
}

/// Every series row of the rule `id` belongs to: every hop of every part. For a rename, which
/// names the rule rather than a stretch of it.
pub(crate) fn rule_ids(conn: &Connection, id: i64) -> Result<Vec<i64>, EditError> {
    Ok(newest(conn, id)?.map(|(_, _, all)| all).unwrap_or_default())
}

/// Where `series.end` writes, and why it refuses: a rule ends as a whole, so any id of it resolves
/// to its newest part, and an end before that part starts would reach back into history.
pub(crate) fn end_target(conn: &Connection, id: i64, until_on: Option<&str>) -> Result<Vec<i64>, EditError> {
    let Some((l, q, _)) = newest(conn, id)? else {
        return Err(fail("not_found", format!("no such series: {id}")));
    };
    if let Some(u) = until_on.and_then(day) {
        if u < l.first().start() {
            if q.is_some() {
                let today = chrono::Local::now().date_naive();
                let verb = if l.first().start() > today { "changes" } else { "changed" };
                return Err(fail(
                    "overlaps_part",
                    format!(
                        "{} {verb} on {}: end it on or after that day, or undo the change first",
                        l.first().description,
                        dmy(l.first().start())
                    ),
                ));
            }
            return Err(bad(format!("it would end on {}, before it starts on {}", dmy(u), dmy(l.first().start()))));
        }
    }
    Ok(l.ids())
}

/// For series.list: the rule each row belongs to, and whether the row is in its newest part.
pub(crate) fn lineage_and_latest(conn: &Connection) -> Result<HashMap<i64, (i64, bool)>, EditError> {
    let hops = load_hops(conn)?;
    let mut out = HashMap::new();
    for (lineage, parts) in rules_of(parts_of(&hops)) {
        let n = parts.len();
        for (i, p) in parts.iter().enumerate() {
            for id in p.ids() {
                out.insert(id, (lineage, i + 1 == n));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// series.review
// ---------------------------------------------------------------------------------------------

fn pct_text(e15: Option<i64>) -> Option<String> {
    e15.map(|v| {
        // e15 of a fraction: 1% is 1e13.
        let s = format!("{:.4}", v as f64 / 1e13);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    })
}

pub(crate) fn review(conn: &mut Connection, params: &Value) -> Result<Value, EditError> {
    let as_of = opt_date(params, "as_of")?.ok_or_else(|| bad("as_of must be YYYY-MM-DD"))?;
    let as_of_s = as_of.to_string();
    let lookback_from = (as_of - Duration::days(LOOKBACK_DAYS)).to_string();
    let tx = conn.transaction()?; // for a consistent read; rolled back
    let snap = forecast::load::snapshot(&tx, as_of)?;
    let hops = load_hops(&tx)?;
    let accounts = load_accounts(&tx)?;
    let scenarios = load_scenarios(&tx)?;
    let all_ids: Vec<i64> = hops.iter().map(|h| h.id).collect();
    let claims = claims_for(&tx, &all_ids)?;
    let adjustments = adjustments_for(&tx, &all_ids)?;

    // What each claimed payment moved on each account, to say "last paid 900.00".
    let mut paid: HashMap<i64, HashMap<i64, i64>> = HashMap::new();
    {
        let mut st = tx.prepare(
            "SELECT p.txn_id, p.account_id, SUM(p.amount_minor) FROM posting p
               JOIN txn t ON t.id = p.txn_id WHERE t.series_id IS NOT NULL GROUP BY p.txn_id, p.account_id",
        )?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
            let (t, a, v) = r?;
            paid.entry(t).or_default().insert(a, v);
        }
    }

    let name = |id: i64| accounts.get(&id).map(|a| a.name.clone()).unwrap_or_default();
    let rules = rules_of(parts_of(&hops));
    let lineage_of: HashMap<i64, i64> = rules
        .iter()
        .flat_map(|(key, parts)| parts.iter().flat_map(|p| p.ids()).map(move |id| (id, *key)))
        .collect();

    // Cancels, by the rule they cancel.
    let mut cancelled_in: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut cancels_by_scenario: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
    let mut baseline_rows: Vec<(Option<String>, String, Value)> = Vec::new();
    let mut what_if_rows: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
    let mut totals: BTreeMap<&str, i64> = BTreeMap::new();
    for key in ["running", "not_started", "ended", "what_if", "cancels", "changed", "no_end", "card_rules", "interest_rules"] {
        totals.insert(key, 0);
    }
    for parts in rules.values() {
        let l = parts.last().expect("a rule has a part");
        if !l.is_cancel() {
            continue;
        }
        let target = l.first().supersedes_id.unwrap_or(0);
        let target_lineage = lineage_of.get(&target).copied().unwrap_or(target);
        let target_rule = rules.get(&target_lineage).and_then(|p| p.last());
        let scenario = l.first().scenario_id.unwrap_or(0);
        let scenario_name = scenarios.get(&scenario).cloned().unwrap_or_default();
        cancelled_in.entry(target_lineage).or_default().push(json!({
            "scenario_id": scenario, "scenario": scenario_name, "cancel_id": l.first().id,
        }));
        cancels_by_scenario.entry(scenario).or_default().push(json!({
            "id": l.first().id,
            "target_lineage_id": target_lineage,
            "target": target_rule.map(|p| p.first().description.clone()).unwrap_or_default(),
            "chain_len": target_rule.filter(|p| p.is_chain()).map(|p| p.hops.len()),
            "phrase": target_rule.map(|p| recur::describe(&p.first().rrule, p.first().start())),
        }));
        *totals.get_mut("cancels").unwrap() += 1;
    }

    let mut monthly: BTreeMap<String, (i128, i128)> = BTreeMap::new();

    for (lineage, parts) in &rules {
        let l = parts.last().expect("a rule has a part");
        if l.is_cancel() {
            continue;
        }
        let first = l.first();
        let shape = shape_of(l);
        let members: HashSet<i64> = parts.iter().flat_map(Part::ids).collect();
        let member_ids: Vec<i64> = members.iter().copied().collect();
        let scenario = first.scenario_id;
        let (sub, proj) = project_rule(&snap, &members, scenario, as_of);
        let mut warnings: Vec<Value> = Vec::new();
        let rule_error = proj.as_ref().err().cloned();
        if let Some(e) = &rule_error {
            warnings.push(json!({ "level": "error", "text": format!("the rule doesn't expand ({e}): every forecast fails until it is fixed") }));
        }

        let route: Vec<Value> = shape
            .route
            .iter()
            .map(|id| {
                let a = accounts.get(id);
                json!({
                    "account_id": id, "name": name(*id),
                    "kind": a.map(|a| a.kind.clone()), "currency": a.map(|a| a.currency.clone()),
                    "closed": a.is_some_and(|a| a.closed), "system": a.is_some_and(|a| a.system),
                })
            })
            .collect();
        let hop_rows: Vec<Value> = l
            .hops
            .iter()
            .enumerate()
            .map(|(k, h)| {
                let primary = h.legs.iter().find(|x| x.role == "primary");
                json!({
                    "id": h.id, "seq": k,
                    "amount_minor": shape.magnitudes.get(k).copied().or(primary.map(|p| p.amount_minor.abs())),
                    "primary_negative": primary.is_some_and(|p| p.amount_minor < 0),
                })
            })
            .collect();
        let legs: Vec<Value> = l
            .hops
            .iter()
            .map(|h| {
                Value::Array(
                    h.legs
                        .iter()
                        .map(|x| json!({ "account_id": x.account_id, "name": name(x.account_id), "role": x.role, "amount_minor": x.amount_minor }))
                        .collect(),
                )
            })
            .collect();

        // Direction: from the kinds at the two ends.
        let kind_of = |id: Option<&i64>| id.and_then(|i| accounts.get(i)).map(|a| a.kind.clone()).unwrap_or_default();
        let balance = |k: &str| k == "asset" || k == "liability";
        let direction = if !shape.two_leg {
            "other"
        } else {
            let (a, b) = (kind_of(shape.route.first()), kind_of(shape.route.last()));
            match (balance(&a), balance(&b)) {
                (true, true) => "transfer",
                (true, false) => "out",
                (false, true) => "in",
                _ => "other",
            }
        };

        // Warnings about the stored rows themselves.
        let shared = |h: &Hop| (h.description.clone(), h.rrule.clone(), h.dtstart.clone(), h.until_on.clone(), h.weekend_rule.clone(), h.scenario_id);
        if l.hops.iter().any(|h| shared(h) != shared(first)) {
            warnings.push(json!({ "level": "warn", "text": "the legs of this chain disagree about its name, rule or dates" }));
        }
        for h in &l.hops {
            let mut sums: BTreeMap<&str, i64> = BTreeMap::new();
            for x in &h.legs {
                *sums.entry(x.currency.as_str()).or_insert(0) += x.amount_minor;
            }
            if sums.values().any(|v| *v != 0) {
                warnings.push(json!({ "level": "warn", "text": "its legs don't add up to zero, so every payment it makes is unbalanced" }));
                break;
            }
        }
        for acc in l.hops.iter().flat_map(|h| h.legs.iter()).map(|x| x.account_id).collect::<BTreeSet<_>>() {
            if let Some(a) = accounts.get(&acc) {
                if a.system {
                    warnings.push(json!({ "level": "warn", "text": format!("{} is a system account", a.name) }));
                }
                if a.closed {
                    warnings.push(json!({ "level": "warn", "text": format!("{} is closed", a.name) }));
                }
            }
        }

        // Recorded payments.
        let rule_claims: Vec<&Claim> = claims.iter().filter(|c| members.contains(&c.series_id)).collect();
        let first_ids: HashSet<i64> = parts.iter().map(|p| p.first().id).collect();
        let primary_of: HashMap<i64, i64> = parts
            .iter()
            .flat_map(|p| p.hops.iter())
            .filter_map(|h| h.legs.iter().find(|x| x.role == "primary").map(|x| (h.id, x.account_id)))
            .collect();
        let first_claims: Vec<&&Claim> = rule_claims.iter().filter(|c| first_ids.contains(&c.series_id)).collect();
        let last = first_claims.iter().max_by(|a, b| (&a.occurrence_on, a.txn_id).cmp(&(&b.occurrence_on, b.txn_id))).map(|c| {
            let amount = primary_of.get(&c.series_id).and_then(|acc| paid.get(&c.txn_id).and_then(|m| m.get(acc))).copied();
            json!({ "txn_id": c.txn_id, "occurrence_on": c.occurrence_on, "occurred_on": c.occurred_on, "amount_minor": amount })
        });
        let mut ahead: BTreeMap<String, (String, Vec<i64>)> = BTreeMap::new();
        for c in &rule_claims {
            if c.occurrence_on > as_of_s {
                let e = ahead.entry(c.occurrence_on.clone()).or_insert((c.occurred_on.clone(), Vec::new()));
                e.1.push(c.txn_id);
            }
        }

        // Adjustments still able to change a forecast, per slot across hops.
        let mut overrides: BTreeMap<(i64, String), Vec<&Adjustment>> = BTreeMap::new();
        for a in adjustments.iter().filter(|a| members.contains(&a.series_id) && a.occurrence_on >= lookback_from) {
            let Some(part) = parts.iter().find(|p| p.ids().contains(&a.series_id)) else { continue };
            // One left behind after its part's end has no effect; it is listed with that part.
            if part.first().until_on.as_ref().is_some_and(|u| &a.occurrence_on > u) {
                continue;
            }
            let part_first = part.first().id;
            overrides.entry((part_first, a.occurrence_on.clone())).or_default().push(a);
        }
        let override_rows: Vec<Value> = overrides
            .iter()
            .map(|((part_first, date), rows)| {
                let part = parts.iter().find(|p| p.first().id == *part_first);
                let amounts: Vec<Option<i64>> = part
                    .map(|p| p.hops.iter().map(|h| rows.iter().find(|r| r.series_id == h.id).and_then(|r| r.amount_minor.map(i64::abs))).collect())
                    .unwrap_or_default();
                json!({
                    "occurrence_on": date, "action": rows[0].action, "word": action_word(rows[0]),
                    "moved_to": rows[0].moved_to, "amounts": amounts,
                    "description": rows[0].description, "series_ids": rows.iter().map(|r| r.series_id).collect::<Vec<_>>(),
                })
            })
            .collect();

        // Stray: a payment or adjustment on a date its part's rule never produces.
        let mut stray: Vec<Value> = Vec::new();
        for p in parts {
            let ids = p.ids();
            let keys: Vec<(i64, &str, &str)> = rule_claims
                .iter()
                .filter(|c| ids.contains(&c.series_id))
                .map(|c| (c.series_id, c.occurrence_on.as_str(), "recorded"))
                .chain(adjustments.iter().filter(|a| ids.contains(&a.series_id)).map(|a| (a.series_id, a.occurrence_on.as_str(), "override")))
                .collect();
            let dates: BTreeSet<NaiveDate> = keys.iter().filter_map(|k| day(k.1)).collect();
            if let (Some(&lo), Some(&hi)) = (dates.first(), dates.last()) {
                if let Ok(have) = slots(&p.first().rrule, p.first().start(), lo, hi) {
                    let mut seen = BTreeSet::new();
                    for (sid, on, kind) in keys {
                        if day(on).is_some_and(|d| !have.contains(&d)) && seen.insert((on, kind)) {
                            stray.push(json!({ "series_id": sid, "occurrence_on": on, "kind": kind }));
                        }
                    }
                }
            }
        }
        if !stray.is_empty() {
            warnings.push(json!({
                "level": "warn",
                "text": format!("{} date{} recorded or adjusted that the rule never produces", stray.len(), if stray.len() == 1 { " is" } else { "s are" }),
            }));
        }

        // The forecast of this rule.
        let pts = proj.as_ref().map(|p| points(&sub, p)).unwrap_or_default();
        let mut next: Vec<Value> = Vec::new();
        let mut due: Vec<Value> = Vec::new();
        for p in pts.values().filter(|p| p.seq == 0) {
            let row = json!({
                "series_id": p.series_id, "occurrence_on": p.occurrence_on, "value_on": p.value_on,
                "amount_minor": p.amount_minor, "moved": p.value_on != p.occurrence_on, "amended": p.amended,
            });
            // Owed when the money should already have moved, which is what UPCOMING calls due too.
            if p.value_on < as_of_s {
                due.push(row);
            } else if next.len() < 6 {
                next.push(row);
            }
        }
        next.sort_by(|a, b| a["value_on"].as_str().cmp(&b["value_on"].as_str()));
        let agg = proj.as_ref().ok().map(|p| analysis::commitment_aggregates(&sub, &p.occurrences));
        let (count, total) = agg
            .as_ref()
            .and_then(|m| m.get(lineage).or_else(|| m.values().next()))
            .map(|a| (a.count, a.annual))
            .unwrap_or((0, 0));
        let monthly_minor = crate::money::round_half_away(total, 12).unwrap_or(0);

        let group = if first.until().is_some_and(|u| u < as_of) {
            "ended"
        } else if parts[0].first().start() > as_of {
            "not_started"
        } else {
            "running"
        };
        let parts_json: Vec<Value> = parts
            .iter()
            .map(|p| {
                let ps = shape_of(p);
                let pid = p.ids();
                json!({
                    "ids": pid,
                    "dtstart": p.first().dtstart,
                    "until_on": p.first().until_on,
                    "rrule": p.first().rrule,
                    "phrase": recur::describe(&p.first().rrule, p.first().start()),
                    "weekend_rule": p.first().weekend_rule,
                    "amounts": ps.magnitudes,
                    "route_names": ps.route.iter().map(|id| name(*id)).collect::<Vec<_>>(),
                    "recorded": rule_claims.iter().filter(|c| c.series_id == p.first().id).count(),
                    "left_behind": adjustments
                        .iter()
                        .filter(|a| pid.contains(&a.series_id) && p.first().until_on.as_ref().is_some_and(|u| &a.occurrence_on > u))
                        .map(|a| (a.occurrence_on.clone(), json!({ "occurrence_on": a.occurrence_on, "action": a.action, "word": action_word(a) })))
                        .collect::<BTreeMap<_, _>>()
                        .into_values()
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let _ = member_ids;

        let replaces = if scenario.is_some() {
            first.supersedes_id.map(|t| {
                let tl = lineage_of.get(&t).copied().unwrap_or(t);
                json!({ "lineage_id": tl, "description": rules.get(&tl).and_then(|p| p.last()).map(|p| p.first().description.clone()) })
            })
        } else {
            None
        };
        let custom_reason = if shape.two_leg {
            Value::Null
        } else {
            let n: usize = l.hops.iter().map(|h| h.legs.len()).sum();
            json!(format!("this rule has {n} legs of its own; its amounts and accounts can't be edited here"))
        };

        let rule = json!({
            "lineage_id": lineage,
            "scenario_id": scenario,
            "scenario": scenario.and_then(|s| scenarios.get(&s).cloned()),
            "group": group,
            "description": first.description,
            "currency": if shape.currency.is_empty() { Value::Null } else { json!(shape.currency) },
            "direction": direction,
            "shape": if shape.two_leg { "two_leg" } else { "custom" },
            "chain_len": if l.is_chain() { json!(l.hops.len()) } else { Value::Null },
            "current": {
                "ids": l.ids(),
                "chain_id": first.chain_id,
                "dtstart": first.dtstart,
                "until_on": first.until_on,
                "rrule": first.rrule,
                "phrase": recur::describe(&first.rrule, first.start()),
                "weekend_rule": first.weekend_rule,
                "first_slot": first_slot(first).map(|d| d.to_string()),
                "route": route,
                "hops": hop_rows,
                "legs": legs,
            },
            "parts": parts_json,
            "editable": {
                "whole": true,
                "from": scenario.is_none(),
                "money": shape.two_leg,
                "start": parts.len() == 1,
                "reason": custom_reason,
            },
            "next": next,
            "due": due,
            "next_12m": { "count": count, "total_minor": i64::try_from(total).unwrap_or(0), "monthly_minor": monthly_minor },
            "recorded": {
                "count": first_claims.len(),
                "last": last,
                "ahead": ahead.into_iter().map(|(on, (paid_on, ids))| json!({ "occurrence_on": on, "occurred_on": paid_on, "txn_ids": ids })).collect::<Vec<_>>(),
            },
            "overrides": override_rows,
            "stray": stray,
            "cancelled_in": cancelled_in.remove(lineage).unwrap_or_default(),
            "replaces": replaces,
            "warnings": warnings,
            "rule_error": rule_error,
        });

        match scenario {
            None => {
                *totals.get_mut(group).unwrap() += 1;
                if parts.len() > 1 {
                    *totals.get_mut("changed").unwrap() += 1;
                }
                if group != "ended" && first.until_on.is_none() {
                    *totals.get_mut("no_end").unwrap() += 1;
                }
                // Out and in by the rule's DIRECTION, not by the sign of its primary leg: the
                // create form writes a salary as money leaving the Salary account, which is money
                // arriving in the household. A transfer between two of its own accounts is neither.
                if group != "ended" && !shape.currency.is_empty() {
                    let e = monthly.entry(shape.currency.clone()).or_insert((0, 0));
                    let m = monthly_minor.unsigned_abs() as i128;
                    match direction {
                        "out" => e.0 -= m,
                        "in" => e.1 += m,
                        "transfer" => {}
                        _ if monthly_minor < 0 => e.0 -= m,
                        _ => e.1 += m,
                    }
                }
                let next_on = rule["next"].get(0).and_then(|n| n["value_on"].as_str()).map(str::to_string);
                baseline_rows.push((next_on, first.description.to_lowercase(), rule));
            }
            Some(s) => {
                *totals.get_mut("what_if").unwrap() += 1;
                what_if_rows.entry(s).or_default().push(rule);
            }
        }
    }

    // Running rules by their next payment, then the rest by name.
    baseline_rows.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(x), Some(y)) => x.cmp(y).then(a.1.cmp(&b.1)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });

    let mut what_if: Vec<Value> = Vec::new();
    let scenario_ids: BTreeSet<i64> = what_if_rows.keys().chain(cancels_by_scenario.keys()).copied().collect();
    for s in scenario_ids {
        what_if.push(json!({
            "scenario_id": s,
            "name": scenarios.get(&s).cloned().unwrap_or_default(),
            "rules": what_if_rows.remove(&s).unwrap_or_default(),
            "cancels": cancels_by_scenario.remove(&s).unwrap_or_default(),
        }));
    }

    // Card and loan rules: listed so a "Payment" in UPCOMING can be traced to its rule.
    let mut card_rules: Vec<Value> = Vec::new();
    {
        let mut st = tx.prepare(
            "SELECT id, account_id, from_account_id, amount_kind, fixed_minor, pct_e15, floor_minor, cap_minor,
                    level_payment_minor, term_periods, interest_rule_id, due_offset_days, rrule, dtstart,
                    until_on, scenario_id
               FROM payment_rule ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?, r.get::<_, Option<i64>>(8)?, r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<i64>>(10)?, r.get::<_, Option<i64>>(11)?, r.get::<_, String>(12)?,
                r.get::<_, String>(13)?, r.get::<_, Option<String>>(14)?, r.get::<_, Option<i64>>(15)?,
            ))
        })?;
        for row in rows {
            let (id, acc, from, kind, fixed, pct, floor, cap, level, term, irule, due_off, rrule, dtstart, until, scen) = row?;
            let start = day(&dtstart).unwrap_or_default();
            let rule_error = RRuleCrate.expand(&rrule, start, None, start, start).err().map(|e| e.to_string());
            card_rules.push(json!({
                "id": id, "account_id": acc, "account": name(acc), "from_account_id": from, "from_account": name(from),
                "currency": accounts.get(&acc).map(|a| a.currency.clone()),
                "amount_kind": kind, "fixed_minor": fixed, "pct": pct_text(pct), "floor_minor": floor, "cap_minor": cap,
                "level_payment_minor": level, "term_periods": term, "interest_rule_id": irule, "due_offset_days": due_off,
                "rrule": rrule, "phrase": recur::describe(&rrule, start), "dtstart": dtstart, "until_on": until,
                "scenario_id": scen, "scenario": scen.and_then(|s| scenarios.get(&s).cloned()), "rule_error": rule_error,
            }));
        }
    }
    let mut interest_rules: Vec<Value> = Vec::new();
    {
        let mut st = tx.prepare(
            "SELECT r.id, r.account_id, r.counter_account_id, r.shape, r.accrues_on, r.accrual_freq,
                    r.capitalise_rrule, r.capitalise_dtstart, r.grace_period, r.scenario_id,
                    (SELECT p.quoted_rate_e15 FROM interest_rate_period p
                      WHERE p.rule_id = r.id AND p.effective_from <= ?1
                        AND (p.effective_to IS NULL OR p.effective_to > ?1)
                      ORDER BY p.effective_from DESC LIMIT 1),
                    (SELECT p.rate_basis FROM interest_rate_period p
                      WHERE p.rule_id = r.id AND p.effective_from <= ?1
                        AND (p.effective_to IS NULL OR p.effective_to > ?1)
                      ORDER BY p.effective_from DESC LIMIT 1)
               FROM interest_rule r ORDER BY r.id",
        )?;
        let rows = st.query_map([&as_of_s], |r| {
            Ok((
                r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?,
                r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, String>(7)?,
                r.get::<_, i64>(8)?, r.get::<_, Option<i64>>(9)?, r.get::<_, Option<i64>>(10)?, r.get::<_, Option<String>>(11)?,
            ))
        })?;
        for row in rows {
            let (id, acc, counter, shape, accrues, freq, cap_rule, cap_start, grace, scen, rate, basis) = row?;
            let start = day(&cap_start).unwrap_or_default();
            interest_rules.push(json!({
                "id": id, "account_id": acc, "account": name(acc), "counter_account": name(counter),
                "shape": shape, "accrues_on": accrues, "accrual_freq": freq,
                "capitalise_rrule": cap_rule, "phrase": recur::describe(&cap_rule, start),
                "capitalise_dtstart": cap_start, "grace_period": grace == 1,
                "rate_in_force": rate.map(|q| json!({ "quoted": pct_text(Some(q)), "basis": basis })),
                "scenario": scen.and_then(|s| scenarios.get(&s).cloned()),
                "warning": if rate.is_none() { json!(format!("no rate in force on {}: the forecast leaves it out", dmy(as_of))) } else { Value::Null },
            }));
        }
    }
    *totals.get_mut("card_rules").unwrap() = card_rules.len() as i64;
    *totals.get_mut("interest_rules").unwrap() = interest_rules.len() as i64;

    Ok(json!({
        "as_of": as_of_s,
        "window": { "from": as_of_s, "to": horizon(as_of).to_string() },
        "rules": baseline_rows.into_iter().map(|r| r.2).collect::<Vec<_>>(),
        "what_if": what_if,
        "card_rules": card_rules,
        "interest_rules": interest_rules,
        "totals": totals,
        "monthly_by_currency": monthly.iter().map(|(c, (out, inn))| json!({
            "currency": c,
            "out_minor": i64::try_from(*out).unwrap_or(0),
            "in_minor": i64::try_from(*inn).unwrap_or(0),
        })).collect::<Vec<_>>(),
    }))
}
