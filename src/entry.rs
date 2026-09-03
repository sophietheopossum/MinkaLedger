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
        "INSERT INTO txn(occurred_on, description, payee, source, note)
         VALUES(?1, ?2, ?3, 'manual', ?4)",
        rusqlite::params![new.occurred_on, new.description, new.payee, new.note],
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

    let mut stmt = conn.prepare(
        "SELECT p.id, p.account_id, a.name, p.currency, p.amount_minor
           FROM posting p JOIN account a ON a.id = p.account_id
          WHERE p.txn_id = ?1 ORDER BY p.id",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "account_id": r.get::<_, i64>(1)?,
                "account": r.get::<_, String>(2)?,
                "currency": r.get::<_, String>(3)?,
                "amount_minor": r.get::<_, i64>(4)?,
            }))
        })?
        .collect::<Result<_, _>>()?;
    head["postings"] = serde_json::Value::Array(rows);
    Ok(head)
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
        "SELECT p.txn_id, a.name, a.kind, p.currency, p.amount_minor
           FROM posting p JOIN account a ON a.id = p.account_id
          WHERE p.txn_id IN ({list})
          ORDER BY p.txn_id, p.amount_minor"
    ))?;
    let mut by_txn: std::collections::HashMap<i64, Vec<serde_json::Value>> = Default::default();
    for row in st.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            serde_json::json!({
                "account": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?,
                "currency": r.get::<_, String>(3)?,
                "amount_minor": r.get::<_, Minor>(4)?,
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
pub fn balances(conn: &Connection, as_of: Option<&str>) -> Result<Vec<serde_json::Value>, EntryError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.kind, a.currency, COALESCE(SUM(p.amount_minor), 0)
           FROM account a
           LEFT JOIN posting p ON p.account_id = a.id
           LEFT JOIN txn t ON t.id = p.txn_id AND (?1 IS NULL OR t.occurred_on <= ?1)
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

    #[test]
    fn the_book_stays_consistent_after_entry() {
        let mut c = book();
        create(&mut c, &txn(vec![p(1, 250_000), p(2, -250_000)])).unwrap();
        create(&mut c, &txn(vec![p(1, -90_000), p(3, 90_000)])).unwrap();
        let report = crate::db::integrity(&c).unwrap();
        assert_eq!(report["ok"], serde_json::json!(true), "{report}");
    }
}
