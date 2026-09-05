//! The only writer of transactions and postings.
//!
//! Everything that records money goes through `create` -- manual entry now, CSV import and the
//! conversion builder later. Centralising it is what makes the balance invariant a property of the
//! system rather than a habit: there is one place to be wrong, and it is tested.
//!
//! Two rules are enforced here that the database cannot express:
//!   - postings sum to zero WITHIN EACH CURRENCY (SQL has no cross-row CHECK)
//!   - `system = 1` accounts (the FX conversion accounts) are core-written only
//!
//! The rest -- currency matching its account, valid dates, at most one rule link -- is already a
//! constraint in the schema, so this layer does not re-check it. Duplicating a database guarantee in
//! Rust means two rules that can drift apart.

use rusqlite::{Connection, Transaction};
use std::collections::BTreeMap;

use crate::fx::FxError;
use crate::link::{self, LinkError};
use crate::money::Minor;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewPosting {
    pub account_id: i64,
    pub amount_minor: Minor,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewTxn {
    pub occurred_on: String,
    pub description: String,
    #[serde(default)]
    pub payee: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    pub postings: Vec<NewPosting>,
    /// The recurring-payment slot this transaction fills, if any: the forecast stops projecting
    /// that slot once a real transaction claims it. Both or neither (the schema checks).
    #[serde(default)]
    pub series_id: Option<i64>,
    #[serde(default)]
    pub occurrence_on: Option<String>,
}

/// One leg of a payment chain: where the money goes next, on what day, and how much of it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewHop {
    pub to_account: i64,
    pub occurred_on: String,
    pub amount_minor: Minor,
}

/// Money moving through several accounts, entered in one go. It leaves `from_account`; each hop
/// says where it goes next. Every hop becomes an ordinary two-posting payment with its own date
/// and amount -- a fee taken on the way, or a leg that lands days later, is just a hop that says
/// so -- and all of them carry the one `description`, which is what the entry form means by a
/// payment chain.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewChain {
    pub description: String,
    pub from_account: i64,
    pub hops: Vec<NewHop>,
}

/// A conversion's two real legs, for `update`: what left `from_account` and what arrived at
/// `to_account`, both positive. The conversion legs that make each currency balance are built by
/// the core, exactly as `txn.convert` builds them.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewConversion {
    pub from_account: i64,
    pub from_minor: Minor,
    pub to_account: i64,
    pub to_minor: Minor,
}

/// What `update` may change about a payment. An absent field is left as it is, so a caller says
/// only what it means to change and a form that was saved untouched changes nothing.
///
/// `payee` and `note` are optional twice over: absent leaves them alone, `null` clears them.
/// `postings` replaces every leg with the ones given, checked like a new payment's; `conversion`
/// does the same for a cross-currency payment, whose legs a caller cannot write by hand because
/// two of them sit in system accounts. One or the other, never both.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct TxnPatch {
    #[serde(default)]
    pub occurred_on: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "given")]
    pub payee: Option<Option<String>>,
    #[serde(default, deserialize_with = "given")]
    pub note: Option<Option<String>>,
    #[serde(default)]
    pub postings: Option<Vec<NewPosting>>,
    #[serde(default)]
    pub conversion: Option<NewConversion>,
}

/// Serde folds a missing field and an explicit `null` into the same `None`; wrapping the value
/// as it is read keeps them apart, so that `"payee": null` can mean "clear it".
fn given<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<Option<String>>, D::Error> {
    <Option<String> as serde::Deserialize>::deserialize(de).map(Some)
}

#[derive(Debug)]
pub enum EntryError {
    Sql(rusqlite::Error),
    /// The postings do not sum to zero. Carries the per-currency residual so the message can say
    /// what is missing rather than just that something is.
    Unbalanced(BTreeMap<String, Minor>),
    TooFewPostings,
    NoSuchAccount(i64),
    /// A caller tried to post to an FX conversion account. GnuCash locks these too, for the same
    /// reason: a hand-written entry there silently breaks the conversion residual.
    SystemAccount(String),
    NotFound(i64),
    /// A payment chain that cannot be recorded as asked. The message says why in the operator's
    /// terms; nothing of the chain is written.
    Chain(String),
    /// A recurring-payment slot already filled by a real transaction (the id it carries).
    AlreadyRecorded(i64),
    /// An edit that cannot be applied as asked -- a blank description, a day that does not exist,
    /// two ways of restating the legs at once. Nothing is written.
    Invalid(String),
}

impl std::fmt::Display for EntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntryError::Sql(e) => write!(f, "{e}"),
            EntryError::Unbalanced(residual) => {
                let parts: Vec<String> =
                    residual.iter().map(|(c, v)| format!("{c} {v}")).collect();
                write!(f, "postings do not sum to zero: residual {}", parts.join(", "))
            }
            EntryError::TooFewPostings => write!(f, "a transaction needs at least two postings"),
            EntryError::NoSuchAccount(id) => write!(f, "no such account: {id}"),
            EntryError::SystemAccount(n) => {
                write!(f, "{n} is a system account -- only the core may post to it")
            }
            EntryError::NotFound(id) => write!(f, "no such transaction: {id}"),
            EntryError::Chain(msg) => write!(f, "{msg}"),
            EntryError::AlreadyRecorded(id) => {
                write!(f, "that occurrence is already recorded as payment {id}")
            }
            EntryError::Invalid(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<LinkError> for EntryError {
    fn from(e: LinkError) -> Self {
        match e {
            LinkError::Sql(e) => EntryError::Sql(e),
            // Cannot happen inside create_chain -- it links only what it has just written, in
            // order -- but if it ever does, refuse the chain rather than misreport it as a
            // database fault.
            other => EntryError::Chain(other.to_string()),
        }
    }
}

impl From<rusqlite::Error> for EntryError {
    fn from(e: rusqlite::Error) -> Self {
        EntryError::Sql(e)
    }
}

impl From<FxError> for EntryError {
    fn from(e: FxError) -> Self {
        match e {
            FxError::Sql(e) => EntryError::Sql(e),
            // Only SameCurrency can reach here, and it is a refusal in the operator's terms: the
            // two accounts hold the same currency, so the payment is a plain one, not a conversion.
            other => EntryError::Invalid(other.to_string()),
        }
    }
}

/// Look up each posting's account, rejecting unknown and system accounts, and return the currency
/// the account holds. The caller never supplies a currency -- it is a property of the account, so
/// there is no way for the two to disagree.
fn resolve_currencies(tx: &Transaction, postings: &[NewPosting]) -> Result<Vec<String>, EntryError> {
    let mut out = Vec::with_capacity(postings.len());
    for p in postings {
        let row = tx
            .query_row(
                "SELECT currency, system, name FROM account WHERE id = ?1",
                [p.account_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => EntryError::NoSuchAccount(p.account_id),
                other => EntryError::Sql(other),
            })?;
        if row.1 == 1 {
            return Err(EntryError::SystemAccount(row.2));
        }
        out.push(row.0);
    }
    Ok(out)
}

/// Sum per currency and return the residual, empty when balanced.
fn residual(postings: &[NewPosting], currencies: &[String]) -> BTreeMap<String, Minor> {
    let mut sums: BTreeMap<String, Minor> = BTreeMap::new();
    for (p, cur) in postings.iter().zip(currencies) {
        *sums.entry(cur.clone()).or_insert(0) += p.amount_minor;
    }
    sums.retain(|_, v| *v != 0);
    sums
}

pub fn create(conn: &mut Connection, new: &NewTxn) -> Result<i64, EntryError> {
    let tx = conn.transaction()?;
    let id = create_in(&tx, new)?;
    tx.commit()?;
    Ok(id)
}

/// `create` without the transaction, for callers that write several transactions as one unit.
fn create_in(tx: &Transaction, new: &NewTxn) -> Result<i64, EntryError> {
    if new.postings.len() < 2 {
        return Err(EntryError::TooFewPostings);
    }
    let currencies = resolve_currencies(tx, &new.postings)?;
    let residual = residual(&new.postings, &currencies);
    if !residual.is_empty() {
        return Err(EntryError::Unbalanced(residual));
    }

    tx.execute(
        "INSERT INTO txn(occurred_on, description, payee, source, note, series_id, occurrence_on)
         VALUES(?1, ?2, ?3, 'manual', ?4, ?5, ?6)",
        rusqlite::params![
            new.occurred_on,
            new.description,
            new.payee,
            new.note,
            new.series_id,
            new.occurrence_on
        ],
    )?;
    let txn_id = tx.last_insert_rowid();
    {
        let mut stmt = tx.prepare(
            "INSERT INTO posting(txn_id, account_id, currency, amount_minor) VALUES(?1,?2,?3,?4)",
        )?;
        for (p, cur) in new.postings.iter().zip(&currencies) {
            stmt.execute(rusqlite::params![txn_id, p.account_id, cur, p.amount_minor])?;
        }
    }
    Ok(txn_id)
}

/// Record a payment chain: one payment per hop, each linked to the one before, all of them or none.
///
/// Each hop is an ordinary two-posting transaction, so every existing reader -- balances,
/// history, the forecast -- sees plain payments and nothing new has to learn about chains. What
/// ties them together is the link graph the payments panel already draws as a thread: hop N is
/// linked to hop N+1, so following any one of them shows the whole chain and where the money
/// ended up. (A journey could hold the same thing as a named container, but the panel reads
/// links, and a chain typed in one go needs no name, sequence or roles to be declared.)
///
/// ONE CURRENCY END TO END. A hop between currencies is a conversion, which is a different shape
/// of transaction (`fx::build_conversion`) with its own executed rate; it is refused here rather
/// than guessed at, so the operator records that leg as the conversion it is.
///
/// ONLY BALANCE-SHEET ACCOUNTS IN THE MIDDLE. Money passes through places it can sit -- a friend,
/// a wallet, a card -- and an expense or income account in the middle would post a spend and its
/// exact reversal, which nets to nothing but reads as real spending in every report that looks
/// at postings rather than balances. Either end may be anything.
///
/// Returns the transaction ids in hop order.
pub fn create_chain(conn: &mut Connection, chain: &NewChain) -> Result<Vec<i64>, EntryError> {
    if chain.hops.len() < 2 {
        return Err(EntryError::Chain(
            "a chain needs at least one stop between from and to".to_string(),
        ));
    }
    if let Some(hop) = chain.hops.iter().find(|h| h.amount_minor <= 0) {
        return Err(EntryError::Chain(format!(
            "every payment in a chain moves a positive amount, and {} is not one",
            hop.amount_minor
        )));
    }
    let route: Vec<i64> = std::iter::once(chain.from_account)
        .chain(chain.hops.iter().map(|h| h.to_account))
        .collect();
    if route.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(EntryError::Chain(
            "a chain cannot move money from an account to itself".to_string(),
        ));
    }

    let tx = conn.transaction()?;
    // Each hop's date has to be a real day, said the one way SQLite's date() gives back, and the
    // hops have to run forward in time: the link drawn between them says which came first. A
    // single payment leaves the first check to the schema, but a chain has several dates and the
    // refusal must say which one.
    for (n, hop) in chain.hops.iter().enumerate() {
        let canonical: Option<String> =
            tx.query_row("SELECT date(?1)", [&hop.occurred_on], |r| r.get(0))?;
        if canonical.as_deref() != Some(hop.occurred_on.as_str()) {
            return Err(EntryError::Chain(format!(
                "hop {} is on no such day: {}",
                n + 1,
                hop.occurred_on
            )));
        }
        if n > 0 && hop.occurred_on < chain.hops[n - 1].occurred_on {
            return Err(EntryError::Chain(format!(
                "hop {} is dated {} but the hop before it is dated {}: a chain runs forward in time",
                n + 1,
                hop.occurred_on,
                chain.hops[n - 1].occurred_on
            )));
        }
    }
    // Resolving every account up front also rejects unknown and system accounts before anything
    // is written, with the same errors a single payment would give.
    let probe: Vec<NewPosting> = route
        .iter()
        .map(|&account_id| NewPosting { account_id, amount_minor: 0 })
        .collect();
    let currencies = resolve_currencies(&tx, &probe)?;
    if let Some(i) = currencies.iter().position(|c| c != &currencies[0]) {
        let name_of = |id: i64| -> Result<String, rusqlite::Error> {
            tx.query_row("SELECT name FROM account WHERE id = ?1", [id], |r| r.get(0))
        };
        return Err(EntryError::Chain(format!(
            "a chain stays in one currency: {} holds {} but {} holds {} -- record that leg as a conversion",
            name_of(route[0])?,
            currencies[0],
            name_of(route[i])?,
            currencies[i]
        )));
    }
    for &id in &route[1..route.len() - 1] {
        let (name, kind): (String, String) = tx.query_row(
            "SELECT name, kind FROM account WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if kind != "asset" && kind != "liability" {
            return Err(EntryError::Chain(format!(
                "money can only pass through an asset or liability account: {name} is an {kind} account"
            )));
        }
    }

    let mut txn_ids: Vec<i64> = Vec::with_capacity(chain.hops.len());
    let mut from = chain.from_account;
    for hop in &chain.hops {
        let payment = NewTxn {
            occurred_on: hop.occurred_on.clone(),
            description: chain.description.clone(),
            payee: None,
            note: None,
            postings: vec![
                NewPosting { account_id: from, amount_minor: -hop.amount_minor },
                NewPosting { account_id: hop.to_account, amount_minor: hop.amount_minor },
            ],
            series_id: None,
            occurrence_on: None,
        };
        let txn_id = create_in(&tx, &payment)?;
        if let Some(&previous) = txn_ids.last() {
            link::create(&tx, previous, txn_id, None, &hop.occurred_on)?;
        }
        txn_ids.push(txn_id);
        from = hop.to_account;
    }
    tx.commit()?;
    Ok(txn_ids)
}

/// Change a payment in place: any of its date, description, payee, note and legs, all of it or
/// none, and answer with the payment as it now reads.
///
/// IN PLACE, NOT DELETE AND RE-CREATE. The id is what everything else holds on to -- the links
/// that thread it into a chain, the journey it is a member of, the import key that stops a
/// re-imported statement duplicating it, the slot a matched series occurrence claims -- and every
/// one of those would be lost by recording it afresh. The postings are the exception: their ids
/// are referenced by nothing, so replacing the legs replaces the rows, and only when the legs
/// actually differ, so an edit that restated them unchanged leaves them untouched.
///
/// THE SAME RULES AS A NEW PAYMENT. Legs given here go through the checks `create` applies --
/// two at least, balanced per currency, no system accounts -- so an edited payment is never one
/// that could not have been recorded. That also means the legs of a conversion cannot be restated
/// as `postings`: two of them sit in system accounts. `conversion` exists for that, and builds
/// the four legs the way `txn.convert` does. A conversion may become a plain payment this way, and
/// a plain payment a conversion; both are just a payment whose legs changed.
///
/// NOT REFUSED ON PROVENANCE, as `txn.rename` is not: an imported or generated payment is the
/// operator's to correct. What is left alone is what the edit does not mention -- the series slot
/// it may claim (`occurrence_on` is the slot's identity, not the day the money moved) and its
/// source.
pub fn update(conn: &mut Connection, id: i64, patch: &TxnPatch) -> Result<serde_json::Value, EntryError> {
    if patch.postings.is_some() && patch.conversion.is_some() {
        return Err(EntryError::Invalid(
            "give the legs as postings or as a conversion, not both".to_string(),
        ));
    }
    let tx = conn.transaction()?;
    // Existence first: an empty patch on a missing payment is still "no such transaction".
    tx.query_row("SELECT 1 FROM txn WHERE id = ?1", [id], |_| Ok(()))
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => EntryError::NotFound(id),
            other => EntryError::Sql(other),
        })?;

    if let Some(on) = &patch.occurred_on {
        // The schema's CHECK would refuse this too, as a constraint failure naming a column. The
        // operator typed a date, so the refusal should name the date.
        let canonical: Option<String> = tx.query_row("SELECT date(?1)", [on], |r| r.get(0))?;
        if canonical.as_deref() != Some(on.as_str()) {
            return Err(EntryError::Invalid(format!("no such day: {on}")));
        }
        tx.execute("UPDATE txn SET occurred_on = ?2 WHERE id = ?1", rusqlite::params![id, on])?;
    }
    if let Some(desc) = &patch.description {
        // NOT NULL is happy with a blank one, and a payment with no readable label cannot be
        // picked out of a list at all. Trimmed, as txn.rename stores it.
        let desc = desc.trim();
        if desc.is_empty() {
            return Err(EntryError::Invalid("description must not be empty".to_string()));
        }
        tx.execute("UPDATE txn SET description = ?2 WHERE id = ?1", rusqlite::params![id, desc])?;
    }
    // A blank payee or note is the same as none: NULL rather than a row that reads as having one.
    let blank_is_null = |v: &Option<String>| -> Option<String> {
        v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    };
    if let Some(payee) = &patch.payee {
        tx.execute(
            "UPDATE txn SET payee = ?2 WHERE id = ?1",
            rusqlite::params![id, blank_is_null(payee)],
        )?;
    }
    if let Some(note) = &patch.note {
        tx.execute(
            "UPDATE txn SET note = ?2 WHERE id = ?1",
            rusqlite::params![id, blank_is_null(note)],
        )?;
    }

    // The legs to write, as (account, currency, amount), from whichever form the patch gave them.
    let legs: Option<Vec<(i64, String, Minor)>> = if let Some(postings) = &patch.postings {
        if postings.len() < 2 {
            return Err(EntryError::TooFewPostings);
        }
        let currencies = resolve_currencies(&tx, postings)?;
        let residual = residual(postings, &currencies);
        if !residual.is_empty() {
            return Err(EntryError::Unbalanced(residual));
        }
        Some(
            postings
                .iter()
                .zip(currencies)
                .map(|(p, cur)| (p.account_id, cur, p.amount_minor))
                .collect(),
        )
    } else if let Some(c) = &patch.conversion {
        if c.from_minor <= 0 || c.to_minor <= 0 {
            return Err(EntryError::Invalid(
                "a conversion's amounts are what left and what arrived, both positive".to_string(),
            ));
        }
        // Through the same lookup as a new payment's legs, so an unknown or system account is
        // refused with the same words before the conversion accounts are consulted.
        let probe = [
            NewPosting { account_id: c.from_account, amount_minor: -c.from_minor },
            NewPosting { account_id: c.to_account, amount_minor: c.to_minor },
        ];
        resolve_currencies(&tx, &probe)?;
        Some(crate::fx::conversion_legs(&tx, c.from_account, c.from_minor, c.to_account, c.to_minor)?)
    } else {
        None
    };
    if let Some(legs) = legs {
        let mut st = tx.prepare(
            "SELECT account_id, currency, amount_minor FROM posting WHERE txn_id = ?1 ORDER BY id",
        )?;
        let current: Vec<(i64, String, Minor)> = st
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        drop(st);
        if current != legs {
            tx.execute("DELETE FROM posting WHERE txn_id = ?1", [id])?;
            let mut st = tx.prepare(
                "INSERT INTO posting(txn_id, account_id, currency, amount_minor) VALUES(?1,?2,?3,?4)",
            )?;
            for (account_id, currency, amount_minor) in &legs {
                st.execute(rusqlite::params![id, account_id, currency, amount_minor])?;
            }
        }
    }
    tx.commit()?;
    get(conn, id)
}

/// What `record_occurrence` was asked to do with one slot of a recurring payment.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RecordOccurrence {
    pub series_id: i64,
    pub occurrence_on: String,
    /// The day the money actually moved; the slot date when absent.
    #[serde(default)]
    pub occurred_on: Option<String>,
    /// The magnitude that actually moved on the primary leg, when it differs from the template
    /// (and from any amend already on the slot). Signed or not: the template's direction wins.
    #[serde(default)]
    pub amount_minor: Option<Minor>,
    /// For a hop of a recurring chain: record every hop of this wave, each linked to the one
    /// before, as `txn.create_chain` would have. Ignored for a plain series.
    #[serde(default)]
    pub whole_chain: bool,
}

/// Turn one projected occurrence of a recurring payment into the real transaction it became, and
/// claim the slot so the forecast stops projecting it.
///
/// This is the other half of `txn.series_id`/`occurrence_on`: the forecast has always skipped a
/// claimed slot, but nothing ever claimed one, so a paid rent stayed "upcoming" and was counted
/// again in every projection. The legs come from the series template with the slot's amend
/// applied the way the projection applies it (the primary leg takes the amount, the balancing
/// leg the negative of it), or from `amount_minor` when the operator says what really moved.
///
/// A chain is recorded as a chain: every hop of the wave, in order, linked hop to hop, all or
/// none. The hops share the slot date by construction, and the recorded date given applies to
/// all of them -- a hop that landed later is corrected afterwards with `txn.update`, which
/// keeps the claim. An amount given for one hop carries through the others as a difference,
/// exactly as an amend does, so a fee taken on the way stays the same fee.
///
/// Returns the transaction ids written, in hop order.
pub fn record_occurrence(conn: &mut Connection, ask: &RecordOccurrence) -> Result<Vec<i64>, EntryError> {
    let tx = conn.transaction()?;
    let canonical: Option<String> =
        tx.query_row("SELECT date(?1)", [&ask.occurrence_on], |r| r.get(0))?;
    if canonical.as_deref() != Some(ask.occurrence_on.as_str()) {
        return Err(EntryError::Invalid(format!("no such day: {}", ask.occurrence_on)));
    }
    let occurred_on = ask.occurred_on.clone().unwrap_or_else(|| ask.occurrence_on.clone());
    let canonical: Option<String> = tx.query_row("SELECT date(?1)", [&occurred_on], |r| r.get(0))?;
    if canonical.as_deref() != Some(occurred_on.as_str()) {
        return Err(EntryError::Invalid(format!("no such day: {occurred_on}")));
    }

    // The series, and for a chain the whole family in hop order.
    let mut st = tx.prepare(
        "SELECT id, description, chain_id, scenario_id FROM series
          WHERE id = ?1 OR (?2 AND chain_id = (SELECT chain_id FROM series WHERE id = ?1))
          ORDER BY chain_seq, id",
    )?;
    let family: Vec<(i64, String, Option<i64>, Option<i64>)> = st
        .query_map(rusqlite::params![ask.series_id, ask.whole_chain], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    drop(st);
    if family.is_empty() {
        return Err(EntryError::Invalid(format!("no such series: {}", ask.series_id)));
    }
    // A what-if is a question, not a payment. Recording one would put hypothetical money in
    // the book and, since the transaction would reference the scenario's series, stop the
    // scenario from ever being deleted.
    if let Some((_, desc, _, _)) = family.iter().find(|f| f.3.is_some()) {
        return Err(EntryError::Invalid(format!(
            "{desc} belongs to a what-if scenario: it is not money that moves"
        )));
    }

    // Each hop as the projection shows it: its template, its slot's amend, and whether the
    // slot is skipped or already taken. All of it read before anything is written, because the
    // operator's amount travels through a chain as a DIFFERENCE from the hop it was typed on --
    // the same rule series.override applies, so a fee stays a fee -- and that needs the typed
    // hop's own magnitude first.
    struct Hop {
        sid: i64,
        desc: String,
        template: Vec<(i64, Minor, String)>,
        magnitude: Option<Minor>, // the amend, if any
    }
    let mut hops: Vec<Hop> = Vec::with_capacity(family.len());
    for (sid, series_desc, _, _) in &family {
        let taken: Option<i64> = tx
            .query_row(
                "SELECT id FROM txn WHERE series_id = ?1 AND occurrence_on = ?2",
                rusqlite::params![sid, ask.occurrence_on],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = taken {
            return Err(EntryError::AlreadyRecorded(id));
        }
        let (skipped, amended, amended_desc): (bool, Option<Minor>, Option<String>) = tx
            .query_row(
                "SELECT action = 'skip', amount_minor, description FROM series_override
                  WHERE series_id = ?1 AND occurrence_on = ?2",
                rusqlite::params![sid, ask.occurrence_on],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap_or((false, None, None));
        if skipped {
            return Err(EntryError::Invalid(format!(
                "that occurrence of {series_desc} is skipped; reset the skip to record it"
            )));
        }
        let mut st = tx.prepare(
            "SELECT account_id, amount_minor, role FROM series_posting WHERE series_id = ?1 ORDER BY id",
        )?;
        let template: Vec<(i64, Minor, String)> = st
            .query_map([sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        drop(st);
        hops.push(Hop {
            sid: *sid,
            desc: amended_desc.unwrap_or_else(|| series_desc.clone()),
            template,
            magnitude: amended.map(Minor::abs),
        });
    }
    // A hop's projected magnitude: its amend, else its primary leg's template.
    let projected = |h: &Hop| -> Minor {
        h.magnitude.unwrap_or_else(|| {
            h.template.iter().find(|t| t.2 == "primary").map(|t| t.1.abs()).unwrap_or(0)
        })
    };
    let delta: Option<Minor> = match (ask.amount_minor, hops.iter().find(|h| h.sid == ask.series_id)) {
        (Some(a), Some(asked)) => Some(a.abs() - projected(asked)),
        _ => None,
    };

    let mut written: Vec<i64> = Vec::with_capacity(hops.len());
    for (n, hop) in hops.iter().enumerate() {
        let magnitude: Option<Minor> = match delta {
            Some(d) => {
                let m = projected(hop) + d;
                if m < 0 {
                    return Err(EntryError::Invalid(format!(
                        "that would take leg {} of the chain below zero",
                        n + 1
                    )));
                }
                Some(m)
            }
            None => hop.magnitude,
        };
        // The primary leg takes the magnitude in its own direction, other legs are as
        // templated, and the balancing leg absorbs whatever makes the payment sum to zero --
        // the negative of the primary for the usual two legs, and of primary plus the rest for
        // a template that splits the remainder. The same rule the projection applies.
        let leg = |tmpl: Minor, role: &str| -> Minor {
            match (magnitude, role) {
                (Some(a), "primary") => if tmpl < 0 { -a } else { a },
                _ => tmpl,
            }
        };
        let others: Minor = hop
            .template
            .iter()
            .filter(|(_, _, role)| role != "balancing")
            .map(|(_, tmpl, role)| leg(*tmpl, role))
            .sum();
        let postings: Vec<NewPosting> = hop
            .template
            .iter()
            .map(|(account_id, tmpl, role)| {
                let amount_minor = if magnitude.is_some() && role == "balancing" { -others } else { leg(*tmpl, role) };
                NewPosting { account_id: *account_id, amount_minor }
            })
            .collect();
        let new = NewTxn {
            occurred_on: occurred_on.clone(),
            description: hop.desc.clone(),
            payee: None,
            note: None,
            postings,
            series_id: Some(hop.sid),
            occurrence_on: Some(ask.occurrence_on.clone()),
        };
        let id = create_in(&tx, &new)?;
        if let Some(&previous) = written.last() {
            link::create(&tx, previous, id, None, &occurred_on)?;
        }
        written.push(id);
    }
    tx.commit()?;
    Ok(written)
}

/// Delete a transaction. Postings go with it via ON DELETE CASCADE.
pub fn delete(conn: &Connection, id: i64) -> Result<(), EntryError> {
    let n = conn.execute("DELETE FROM txn WHERE id = ?1", [id])?;
    if n == 0 {
        return Err(EntryError::NotFound(id));
    }
    Ok(())
}

pub fn get(conn: &Connection, id: i64) -> Result<serde_json::Value, EntryError> {
    let mut head = conn
        .query_row(
            "SELECT id, occurred_on, description, payee, source, note FROM txn WHERE id = ?1",
            [id],
            |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "occurred_on": r.get::<_, String>(1)?,
                    "description": r.get::<_, String>(2)?,
                    "payee": r.get::<_, Option<String>>(3)?,
                    "source": r.get::<_, String>(4)?,
                    "note": r.get::<_, Option<String>>(5)?,
                }))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => EntryError::NotFound(id),
            other => EntryError::Sql(other),
        })?;

    // `kind` rides along so a reader can tell a conversion's own legs from the real ones without
    // a second lookup -- summarise carries it for the same reason, and an editor that loads a
    // payment from either answer sees the same shape.
    let mut stmt = conn.prepare(
        "SELECT p.id, p.account_id, a.name, a.kind, p.currency, p.amount_minor
           FROM posting p JOIN account a ON a.id = p.account_id
          WHERE p.txn_id = ?1 ORDER BY p.id",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "account_id": r.get::<_, i64>(1)?,
                "account": r.get::<_, String>(2)?,
                "kind": r.get::<_, String>(3)?,
                "currency": r.get::<_, String>(4)?,
                "amount_minor": r.get::<_, i64>(5)?,
            }))
        })?
        .collect::<Result<_, _>>()?;
    head["postings"] = serde_json::Value::Array(rows);
    Ok(head)
}

/// What a payment did to each account it touched: the balance the moment before it and the
/// moment after, for a tooltip on the payment.
///
/// "Before" is every posting on that account that the book orders ahead of this payment -- an
/// earlier day, or the same day and a lower id, which is the order every list in the app shows
/// them in -- so the numbers read the same as scrolling the ledger would. Legs on the same
/// account are folded into one line, and a conversion's own legs are left out, as everywhere
/// else: the conversion accounts are the book's machinery, not somewhere money is kept.
pub fn context(conn: &Connection, id: i64) -> Result<serde_json::Value, EntryError> {
    let occurred_on: String = conn
        .query_row("SELECT occurred_on FROM txn WHERE id = ?1", [id], |r| r.get(0))
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => EntryError::NotFound(id),
            other => EntryError::Sql(other),
        })?;
    let mut st = conn.prepare(
        "SELECT a.id, a.name, a.kind, a.currency, SUM(p.amount_minor),
                (SELECT COALESCE(SUM(q.amount_minor), 0)
                   FROM posting q JOIN txn u ON u.id = q.txn_id
                  WHERE q.account_id = a.id
                    AND (u.occurred_on < ?2 OR (u.occurred_on = ?2 AND u.id < ?1)))
           FROM posting p JOIN account a ON a.id = p.account_id
          WHERE p.txn_id = ?1 AND a.kind <> 'conversion'
          GROUP BY a.id
          ORDER BY SUM(p.amount_minor), a.name",
    )?;
    let legs: Vec<serde_json::Value> = st
        .query_map(rusqlite::params![id, occurred_on], |r| {
            let moved: Minor = r.get(4)?;
            let before: Minor = r.get(5)?;
            Ok(serde_json::json!({
                "account_id": r.get::<_, i64>(0)?,
                "account": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?,
                "currency": r.get::<_, String>(3)?,
                "amount_minor": moved,
                "before_minor": before,
                "after_minor": before + moved,
            }))
        })?
        .collect::<Result<_, _>>()?;
    Ok(serde_json::json!({ "id": id, "occurred_on": occurred_on, "legs": legs }))
}

/// History, newest first. `from`/`to` are inclusive ISO dates; both optional.
pub fn list(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
    limit: i64,
) -> Result<Vec<serde_json::Value>, EntryError> {
    let mut stmt = conn.prepare(
        "SELECT id, occurred_on, description, payee, source
           FROM txn
          WHERE (?1 IS NULL OR occurred_on >= ?1)
            AND (?2 IS NULL OR occurred_on <= ?2)
          ORDER BY occurred_on DESC, id DESC
          LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![from, to, limit], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "occurred_on": r.get::<_, String>(1)?,
                "description": r.get::<_, String>(2)?,
                "payee": r.get::<_, Option<String>>(3)?,
                "source": r.get::<_, String>(4)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One transaction, with enough of its postings to be recognisable in a list or a graph
/// node. Shared by the payment browser and the link graph so a payment looks the same in
/// both, and carries its link count so a browser can show what is already threaded.
pub fn summarise(conn: &Connection, ids: &[i64]) -> Result<Vec<serde_json::Value>, EntryError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let mut st = conn.prepare(&format!(
        "SELECT t.id, t.occurred_on, t.description, t.payee, t.source,
                (SELECT COUNT(*) FROM txn_link l
                  WHERE l.from_txn_id = t.id OR l.to_txn_id = t.id)
           FROM txn t WHERE t.id IN ({list}) ORDER BY t.occurred_on, t.id"
    ))?;
    let mut rows: Vec<serde_json::Value> = st
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "occurred_on": r.get::<_, String>(1)?,
                "description": r.get::<_, String>(2)?,
                "payee": r.get::<_, Option<String>>(3)?,
                "source": r.get::<_, String>(4)?,
                "links": r.get::<_, i64>(5)?,
            }))
        })?
        .collect::<Result<_, _>>()?;

    // Postings come as a second pass rather than a join: joining would fan each transaction out
    // into one row per posting and the caller would have to regroup them.
    let mut st = conn.prepare(&format!(
        "SELECT p.txn_id, p.account_id, a.name, a.kind, p.currency, p.amount_minor
           FROM posting p JOIN account a ON a.id = p.account_id
          WHERE p.txn_id IN ({list})
          ORDER BY p.txn_id, p.amount_minor"
    ))?;
    let mut by_txn: std::collections::HashMap<i64, Vec<serde_json::Value>> = Default::default();
    for row in st.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            serde_json::json!({
                "account_id": r.get::<_, i64>(1)?,
                "account": r.get::<_, String>(2)?,
                "kind": r.get::<_, String>(3)?,
                "currency": r.get::<_, String>(4)?,
                "amount_minor": r.get::<_, Minor>(5)?,
            }),
        ))
    })? {
        let (t, p) = row?;
        by_txn.entry(t).or_default().push(p);
    }
    for r in &mut rows {
        let id = r["id"].as_i64().unwrap_or(0);
        r["postings"] = serde_json::json!(by_txn.remove(&id).unwrap_or_default());
    }
    Ok(rows)
}

/// The payment browser's query: search text, a date window, one account, with paging.
///
/// Separate from `list` rather than replacing it: `list` is the cheap "what happened" used by the
/// importer and the tests, while this fans out into postings and link counts for every row and is
/// only worth that when something is actually going to display them.
///
/// Search matches description OR payee, case-insensitively. The account filter is a subquery on
/// posting rather than a join, so a transaction touching the account twice still yields one row.
pub fn browse(
    conn: &Connection,
    search: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    account_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<serde_json::Value, EntryError> {
    let pattern = search.map(|s| format!("%{}%", s.trim().to_lowercase()));
    let mut stmt = conn.prepare(
        "SELECT t.id FROM txn t
          WHERE (?1 IS NULL OR lower(t.description) LIKE ?1
                            OR lower(COALESCE(t.payee, '')) LIKE ?1)
            AND (?2 IS NULL OR t.occurred_on >= ?2)
            AND (?3 IS NULL OR t.occurred_on <= ?3)
            AND (?4 IS NULL OR EXISTS (SELECT 1 FROM posting p
                                        WHERE p.txn_id = t.id AND p.account_id = ?4))
          ORDER BY t.occurred_on DESC, t.id DESC
          LIMIT ?5 OFFSET ?6",
    )?;
    let ids: Vec<i64> = stmt
        .query_map(
            rusqlite::params![pattern, from, to, account_id, limit, offset],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?;

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM txn t
          WHERE (?1 IS NULL OR lower(t.description) LIKE ?1
                            OR lower(COALESCE(t.payee, '')) LIKE ?1)
            AND (?2 IS NULL OR t.occurred_on >= ?2)
            AND (?3 IS NULL OR t.occurred_on <= ?3)
            AND (?4 IS NULL OR EXISTS (SELECT 1 FROM posting p
                                        WHERE p.txn_id = t.id AND p.account_id = ?4))",
        rusqlite::params![pattern, from, to, account_id],
        |r| r.get(0),
    )?;

    // summarise orders by date ASCENDING; the browser wants newest first, which is the order the
    // id query already established.
    let mut rows = summarise(conn, &ids)?;
    rows.sort_by_key(|r| {
        ids.iter().position(|i| *i == r["id"].as_i64().unwrap_or(0)).unwrap_or(usize::MAX)
    });

    Ok(serde_json::json!({
        "rows": rows,
        "total": total,
        "offset": offset,
        "limit": limit,
    }))
}

/// Distinct descriptions, commonest first: what a "pick a description you have used before" box
/// offers.
///
/// Ordered by USE COUNT rather than recency, because the point is the handful of labels typed
/// every month -- recency alone would put a one-off from yesterday above the weekly shop. `last_on`
/// breaks ties toward the one still in use, and the description itself breaks the remaining ties so
/// that two labels used equally often do not swap places between identical calls: a suggestion list
/// that reshuffles under the cursor is worse than one in a slightly arbitrary order.
///
/// Grouping is on the description EXACTLY as stored, so "Rent" and "RENT" are two entries. They are
/// two different strings in the book, and the caller inserts whichever it is given verbatim --
/// folding them would mean choosing a spelling on the operator's behalf and quietly hiding that the
/// book holds both.
///
/// `prefix` is a misnomer inherited from the caller: it matches anywhere in the description, so
/// typing "tesco" finds "Weekly TESCO shop". A search box that only matched the front would miss
/// the label whenever the memorable word is not the first one.
///
/// Nothing is filtered out by provenance. An imported or generated description is still a real
/// label from this book, and if it has been used often enough to reach the top of this list it is
/// exactly the one worth offering.
pub fn descriptions(
    conn: &Connection,
    prefix: Option<&str>,
    limit: i64,
) -> Result<Vec<serde_json::Value>, EntryError> {
    // `%` and `_` are LIKE wildcards, and this pattern is rebuilt from the box on every keystroke:
    // without escaping, typing "50% off" would match every description in the book and the list
    // would appear to ignore what was typed. ESCAPE names the escape character, since SQLite has no
    // default one.
    //
    // The case fold is left to LIKE, which is why neither side lowercases. Folding here AND in SQL
    // looked symmetrical and was not: Rust's `to_lowercase` is Unicode-aware while stock SQLite's
    // `lower()` folds ASCII only, so an imported "CAFÉ NERO" became "café nero" on one side and
    // "cafÉ nero" on the other and could never match itself -- typing a label exactly as the book
    // holds it returned nothing at all. Bank statements arrive in capitals routinely, so that was
    // not a corner. LIKE's own fold is ASCII-only too, but it is the SAME fold on both sides, so a
    // description always at least matches itself.
    let pattern = prefix.map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| {
        format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
    });

    let mut stmt = conn.prepare(
        "SELECT description, COUNT(*) AS uses, MAX(occurred_on) AS last_on
           FROM txn
          WHERE (?1 IS NULL OR description LIKE ?1 ESCAPE '\\')
          GROUP BY description
          ORDER BY uses DESC, last_on DESC, description ASC
          LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![pattern, limit], |r| {
            Ok(serde_json::json!({
                "description": r.get::<_, String>(0)?,
                "count": r.get::<_, i64>(1)?,
                "last_on": r.get::<_, String>(2)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Current balance per account, from real postings only. The forecast's starting point.
///
/// `as_of` bounds the postings counted, inclusive. The date has to live on the POSTING join: it
/// used to sit on a second LEFT JOIN to txn, which leaves the posting row in place with a NULL
/// transaction beside it and sums it anyway, so every balance "as of" a date was the balance
/// regardless of date. Nothing noticed while the only caller passed no date.
pub fn balances(conn: &Connection, as_of: Option<&str>) -> Result<Vec<serde_json::Value>, EntryError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.kind, a.currency, COALESCE(SUM(p.amount_minor), 0)
           FROM account a
           LEFT JOIN posting p ON p.account_id = a.id
                AND (?1 IS NULL OR EXISTS (SELECT 1 FROM txn t
                                            WHERE t.id = p.txn_id AND t.occurred_on <= ?1))
          WHERE a.closed = 0
          GROUP BY a.id
          ORDER BY a.kind, a.name",
    )?;
    let rows = stmt
        .query_map([as_of], |r| {
            Ok(serde_json::json!({
                "account_id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?,
                "currency": r.get::<_, String>(3)?,
                "balance_minor": r.get::<_, i64>(4)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Daily closing balances per open account across a window, for drawing history on the chart.
///
/// One point per day an account moved, from `from` to `to` inclusive, plus `opening_minor`: the
/// balance carried in from before `from`, so a line starts at its true level rather than at
/// zero. Days between points carry the previous point forward; the caller draws that however it
/// likes. Closed accounts are left out, as `balances` leaves them out. Postings after `to` are
/// not counted anywhere, so `to` is the date the last point can be trusted to.
pub fn history(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
    ids: Option<&[i64]>,
) -> Result<Vec<serde_json::Value>, EntryError> {
    let wanted = |id: i64| ids.is_none_or(|ids| ids.contains(&id));

    let mut stmt =
        conn.prepare("SELECT id, name, kind, currency FROM account WHERE closed = 0 ORDER BY kind, name")?;
    let accounts: Vec<(i64, String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .filter_map(|row| row.ok())
        .filter(|(id, ..)| wanted(*id))
        .collect();

    let mut stmt = conn.prepare(
        "SELECT p.account_id, t.occurred_on, SUM(p.amount_minor)
           FROM posting p JOIN txn t ON t.id = p.txn_id
          WHERE (?1 IS NULL OR t.occurred_on <= ?1)
          GROUP BY p.account_id, t.occurred_on
          ORDER BY p.account_id, t.occurred_on",
    )?;
    let mut opening: BTreeMap<i64, Minor> = BTreeMap::new();
    let mut running: BTreeMap<i64, Minor> = BTreeMap::new();
    let mut points: BTreeMap<i64, Vec<serde_json::Value>> = BTreeMap::new();
    for row in stmt.query_map([to], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Minor>(2)?))
    })? {
        let (account_id, on, moved) = row?;
        if !wanted(account_id) {
            continue;
        }
        let balance = running.entry(account_id).or_insert(0);
        *balance += moved;
        if from.is_some_and(|f| on.as_str() < f) {
            opening.insert(account_id, *balance);
        } else {
            points
                .entry(account_id)
                .or_default()
                .push(serde_json::json!({ "on": on, "balance_minor": *balance }));
        }
    }

    Ok(accounts
        .into_iter()
        .map(|(id, name, kind, currency)| {
            serde_json::json!({
                "account_id": id,
                "name": name,
                "kind": kind,
                "currency": currency,
                "opening_minor": opening.get(&id).copied().unwrap_or(0),
                "points": points.remove(&id).unwrap_or_default(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql")).unwrap();
        // GBP/EUR/USD/JPY are seeded by the migration itself; only accounts are ours to add.
        conn.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES(1,'Current','asset','GBP');
             INSERT INTO account(id,name,kind,currency) VALUES(2,'Salary','income','GBP');
             INSERT INTO account(id,name,kind,currency) VALUES(3,'Rent','expense','GBP');
             INSERT INTO account(id,name,kind,currency) VALUES(4,'Euro pot','asset','EUR');
             INSERT INTO account(id,name,kind,currency,system) VALUES(9,'Conversion:GBP','conversion','GBP',1);",
        )
        .unwrap();
        conn
    }

    fn p(account_id: i64, amount_minor: i64) -> NewPosting {
        NewPosting { account_id, amount_minor }
    }

    fn txn(postings: Vec<NewPosting>) -> NewTxn {
        NewTxn {
            occurred_on: "2026-08-28".into(),
            description: "test".into(),
            payee: None,
            note: None,
            postings,
            series_id: None,
            occurrence_on: None,
        }
    }

    #[test]
    fn records_a_balanced_transaction() {
        let mut c = book();
        // salary in: income is credited (negative), the asset debited (positive)
        let id = create(&mut c, &txn(vec![p(1, 250_000), p(2, -250_000)])).unwrap();
        let got = get(&c, id).unwrap();
        assert_eq!(got["postings"].as_array().unwrap().len(), 2);
        let bals = balances(&c, None).unwrap();
        let current = bals.iter().find(|b| b["name"] == "Current").unwrap();
        assert_eq!(current["balance_minor"], 250_000);
    }

    #[test]
    fn refuses_an_unbalanced_transaction_and_says_by_how_much() {
        let mut c = book();
        match create(&mut c, &txn(vec![p(1, 250_000), p(2, -249_000)])) {
            Err(EntryError::Unbalanced(r)) => {
                assert_eq!(r.get("GBP"), Some(&1_000)); // a tenner short, named
            }
            other => panic!("expected Unbalanced, got {other:?}"),
        }
        // and nothing was written
        assert_eq!(list(&c, None, None, 100).unwrap().len(), 0);
    }

    #[test]
    fn balance_is_checked_per_currency_not_in_total() {
        let mut c = book();
        // +100 GBP and -100 EUR nets to zero if you ignore currency. It must not be accepted:
        // this is exactly the cross-currency transaction that needs conversion postings.
        match create(&mut c, &txn(vec![p(1, 10_000), p(4, -10_000)])) {
            Err(EntryError::Unbalanced(r)) => {
                assert_eq!(r.get("GBP"), Some(&10_000));
                assert_eq!(r.get("EUR"), Some(&-10_000));
            }
            other => panic!("expected Unbalanced, got {other:?}"),
        }
    }

    #[test]
    fn refuses_a_hand_posting_to_a_conversion_account() {
        let mut c = book();
        match create(&mut c, &txn(vec![p(1, 10_000), p(9, -10_000)])) {
            Err(EntryError::SystemAccount(n)) => assert_eq!(n, "Conversion:GBP"),
            other => panic!("expected SystemAccount, got {other:?}"),
        }
    }

    #[test]
    fn refuses_unknown_accounts_and_stubs() {
        let mut c = book();
        assert!(matches!(
            create(&mut c, &txn(vec![p(1, 100), p(404, -100)])),
            Err(EntryError::NoSuchAccount(404))
        ));
        assert!(matches!(
            create(&mut c, &txn(vec![p(1, 0)])),
            Err(EntryError::TooFewPostings)
        ));
    }

    #[test]
    fn a_multi_leg_split_balances() {
        let mut c = book();
        // one payment covering rent and a bill, from one account
        let id = create(&mut c, &txn(vec![p(1, -90_000), p(3, 75_000), p(3, 15_000)])).unwrap();
        assert_eq!(get(&c, id).unwrap()["postings"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn delete_removes_postings_too() {
        let mut c = book();
        let id = create(&mut c, &txn(vec![p(1, 100), p(2, -100)])).unwrap();
        delete(&c, id).unwrap();
        assert!(matches!(get(&c, id), Err(EntryError::NotFound(_))));
        let n: i64 = c
            .query_row("SELECT count(*) FROM posting WHERE txn_id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "postings must cascade");
        assert!(matches!(delete(&c, id), Err(EntryError::NotFound(_))));
    }

    #[test]
    fn context_gives_each_account_its_balance_either_side_of_the_payment() {
        let mut c = book();
        let mut opening = txn(vec![p(1, 100_000), p(2, -100_000)]);
        opening.occurred_on = "2026-08-01".into();
        create(&mut c, &opening).unwrap();
        let mut rent = txn(vec![p(1, -90_000), p(3, 90_000)]);
        rent.occurred_on = "2026-08-05".into();
        let rent_id = create(&mut c, &rent).unwrap();
        // Same day, later id: ordered after the rent, so not in its "before".
        let mut later = txn(vec![p(1, -1_000), p(3, 1_000)]);
        later.occurred_on = "2026-08-05".into();
        create(&mut c, &later).unwrap();

        let ctx = context(&c, rent_id).unwrap();
        let legs = ctx["legs"].as_array().unwrap();
        assert_eq!(legs.len(), 2);
        let current = legs.iter().find(|l| l["account"] == "Current").unwrap();
        assert_eq!(current["before_minor"], 100_000);
        assert_eq!(current["after_minor"], 10_000);
        let rent_acc = legs.iter().find(|l| l["account"] == "Rent").unwrap();
        assert_eq!(rent_acc["before_minor"], 0);
        assert_eq!(rent_acc["after_minor"], 90_000);
        assert!(matches!(context(&c, 404), Err(EntryError::NotFound(404))));
    }

    #[test]
    fn balances_as_of_a_date_stop_at_that_date() {
        let mut c = book();
        for (on, amount) in [("2026-08-01", 100), ("2026-09-01", 1_000), ("2026-10-01", 10_000)] {
            let mut t = txn(vec![p(1, amount), p(2, -amount)]);
            t.occurred_on = on.into();
            create(&mut c, &t).unwrap();
        }
        let current = |as_of: Option<&str>| -> i64 {
            balances(&c, as_of).unwrap().iter().find(|b| b["name"] == "Current").unwrap()["balance_minor"]
                .as_i64()
                .unwrap()
        };
        assert_eq!(current(None), 11_100);
        assert_eq!(current(Some("2026-09-15")), 1_100, "October is still to come");
        assert_eq!(current(Some("2026-09-01")), 1_100, "inclusive of the day");
        assert_eq!(current(Some("2026-07-01")), 0, "and an account with nothing yet is still listed");
    }

    #[test]
    fn list_filters_by_date_window() {
        let mut c = book();
        for d in ["2026-01-15", "2026-06-15", "2026-12-15"] {
            let mut t = txn(vec![p(1, 100), p(2, -100)]);
            t.occurred_on = d.into();
            create(&mut c, &t).unwrap();
        }
        assert_eq!(list(&c, None, None, 100).unwrap().len(), 3);
        assert_eq!(list(&c, Some("2026-06-01"), None, 100).unwrap().len(), 2);
        assert_eq!(list(&c, Some("2026-02-01"), Some("2026-11-01"), 100).unwrap().len(), 1);
        // newest first
        let all = list(&c, None, None, 100).unwrap();
        assert_eq!(all[0]["occurred_on"], "2026-12-15");
    }

    fn ids_of(c: &Connection, txn: i64) -> Vec<i64> {
        let mut st = c.prepare("SELECT id FROM posting WHERE txn_id = ?1 ORDER BY id").unwrap();
        st.query_map([txn], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    fn balance_of(c: &Connection, name: &str) -> i64 {
        balances(c, None).unwrap().iter().find(|b| b["name"] == name).unwrap()["balance_minor"]
            .as_i64()
            .unwrap()
    }

    #[test]
    fn update_corrects_the_record_under_the_same_id() {
        let mut c = book();
        // Rent paid, but typed against Salary and a tenner short, on the wrong day.
        let id = create(&mut c, &txn(vec![p(1, -89_000), p(2, 89_000)])).unwrap();
        let got = update(
            &mut c,
            id,
            &TxnPatch {
                occurred_on: Some("2026-09-01".into()),
                description: Some("  Rent — flat  ".into()),
                payee: Some(Some("Landlord".into())),
                postings: Some(vec![p(1, -90_000), p(3, 90_000)]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got["id"], id);
        assert_eq!(got["occurred_on"], "2026-09-01");
        assert_eq!(got["description"], "Rent — flat", "stored trimmed");
        assert_eq!(got["payee"], "Landlord");
        assert_eq!(got["source"], "manual", "provenance is not the edit's to change");
        assert_eq!(got["postings"].as_array().unwrap().len(), 2);
        assert_eq!(balance_of(&c, "Current"), -90_000);
        assert_eq!(balance_of(&c, "Rent"), 90_000);
        assert_eq!(balance_of(&c, "Salary"), 0, "the leg moved off the wrong account");
        assert_eq!(
            crate::db::integrity(&c).unwrap()["ok"],
            serde_json::json!(true)
        );

        // Clearing a payee is saying so explicitly; not mentioning it leaves it.
        let got = update(&mut c, id, &TxnPatch::default()).unwrap();
        assert_eq!(got["payee"], "Landlord");
        let got = update(&mut c, id, &TxnPatch { payee: Some(None), ..Default::default() }).unwrap();
        assert_eq!(got["payee"], serde_json::Value::Null);
        let got =
            update(&mut c, id, &TxnPatch { note: Some(Some("  ".into())), ..Default::default() })
                .unwrap();
        assert_eq!(got["note"], serde_json::Value::Null, "a blank note is no note");
    }

    #[test]
    fn update_leaves_legs_alone_when_they_are_restated_unchanged() {
        let mut c = book();
        let id = create(&mut c, &txn(vec![p(1, -90_000), p(3, 90_000)])).unwrap();
        // A later payment holds the highest posting ids, so rewritten legs of the first cannot
        // get their old ids back by rowid reuse and the difference is observable.
        create(&mut c, &txn(vec![p(1, -100), p(2, 100)])).unwrap();
        let before = ids_of(&c, id);
        update(
            &mut c,
            id,
            &TxnPatch {
                description: Some("still rent".into()),
                postings: Some(vec![p(1, -90_000), p(3, 90_000)]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids_of(&c, id), before, "identical legs keep their rows");
        update(&mut c, id, &TxnPatch { postings: Some(vec![p(1, -95_000), p(3, 95_000)]), ..Default::default() })
            .unwrap();
        assert_ne!(ids_of(&c, id), before, "different legs are new rows");
        assert_eq!(ids_of(&c, id).len(), 2);
    }

    #[test]
    fn update_refuses_what_create_refuses_and_writes_nothing() {
        let mut c = book();
        let id = create(&mut c, &txn(vec![p(1, -90_000), p(3, 90_000)])).unwrap();
        let before = get(&c, id).unwrap();
        let legs = |v: Vec<NewPosting>| TxnPatch { postings: Some(v), ..Default::default() };

        assert!(matches!(
            update(&mut c, id, &legs(vec![p(1, -90_000), p(3, 89_000)])),
            Err(EntryError::Unbalanced(_))
        ));
        assert!(matches!(
            update(&mut c, id, &legs(vec![p(1, -90_000), p(9, 90_000)])),
            Err(EntryError::SystemAccount(_))
        ));
        assert!(matches!(
            update(&mut c, id, &legs(vec![p(1, -90_000), p(404, 90_000)])),
            Err(EntryError::NoSuchAccount(404))
        ));
        assert!(matches!(update(&mut c, id, &legs(vec![p(1, 0)])), Err(EntryError::TooFewPostings)));
        // Cross-currency legs are unbalanced per currency, exactly as they would be on create.
        assert!(matches!(
            update(&mut c, id, &legs(vec![p(1, -90_000), p(4, 90_000)])),
            Err(EntryError::Unbalanced(_))
        ));
        assert!(matches!(
            update(&mut c, id, &TxnPatch { occurred_on: Some("2026-02-30".into()), ..Default::default() }),
            Err(EntryError::Invalid(_))
        ));
        assert!(matches!(
            update(&mut c, id, &TxnPatch { description: Some("  ".into()), ..Default::default() }),
            Err(EntryError::Invalid(_))
        ));
        assert!(matches!(
            update(
                &mut c,
                id,
                &TxnPatch {
                    postings: Some(vec![p(1, -1), p(3, 1)]),
                    conversion: Some(NewConversion { from_account: 1, from_minor: 1, to_account: 4, to_minor: 1 }),
                    ..Default::default()
                }
            ),
            Err(EntryError::Invalid(_))
        ));
        assert!(matches!(update(&mut c, 404, &TxnPatch::default()), Err(EntryError::NotFound(404))));

        // A refusal after a field had already been updated inside the transaction must roll the
        // whole edit back: the date here is fine, the legs are not.
        assert!(update(
            &mut c,
            id,
            &TxnPatch {
                occurred_on: Some("2026-09-01".into()),
                postings: Some(vec![p(1, -90_000), p(3, 89_000)]),
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(get(&c, id).unwrap(), before, "nothing of a refused edit is kept");
    }

    #[test]
    fn update_can_make_a_conversion_and_unmake_it() {
        let mut c = book();
        let id = create(&mut c, &txn(vec![p(1, -10_000), p(3, 10_000)])).unwrap();
        // Actually it was money sent to the euro pot: a conversion, which needs its own legs.
        let got = update(
            &mut c,
            id,
            &TxnPatch {
                conversion: Some(NewConversion {
                    from_account: 1,
                    from_minor: 10_000,
                    to_account: 4,
                    to_minor: 11_500,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got["postings"].as_array().unwrap().len(), 4, "two real legs, two conversion legs");
        assert_eq!(balance_of(&c, "Current"), -10_000);
        assert_eq!(balance_of(&c, "Euro pot"), 11_500);
        assert_eq!(balance_of(&c, "Rent"), 0);
        assert_eq!(crate::db::integrity(&c).unwrap()["ok"], serde_json::json!(true));

        // Both sides in one currency is not a conversion, and says so rather than writing legs.
        assert!(matches!(
            update(
                &mut c,
                id,
                &TxnPatch {
                    conversion: Some(NewConversion { from_account: 1, from_minor: 1, to_account: 3, to_minor: 1 }),
                    ..Default::default()
                }
            ),
            Err(EntryError::Invalid(_))
        ));
        assert!(matches!(
            update(
                &mut c,
                id,
                &TxnPatch {
                    conversion: Some(NewConversion { from_account: 1, from_minor: -1, to_account: 4, to_minor: 1 }),
                    ..Default::default()
                }
            ),
            Err(EntryError::Invalid(_))
        ));

        // And back to a plain payment: the conversion legs go with the rest.
        let got = update(&mut c, id, &TxnPatch { postings: Some(vec![p(1, -10_000), p(3, 10_000)]), ..Default::default() })
            .unwrap();
        assert_eq!(got["postings"].as_array().unwrap().len(), 2);
        assert_eq!(balance_of(&c, "Euro pot"), 0);
        assert_eq!(crate::db::integrity(&c).unwrap()["ok"], serde_json::json!(true));
    }

    #[test]
    fn the_book_stays_consistent_after_entry() {
        let mut c = book();
        create(&mut c, &txn(vec![p(1, 250_000), p(2, -250_000)])).unwrap();
        create(&mut c, &txn(vec![p(1, -90_000), p(3, 90_000)])).unwrap();
        let report = crate::db::integrity(&c).unwrap();
        assert_eq!(report["ok"], serde_json::json!(true), "{report}");
    }
}
