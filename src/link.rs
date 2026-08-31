//! Free-form links between transactions, and the graph you get by following them.
//!
//! A JOURNEY IS A CONTAINER; THIS IS A GRAPH. `journey` models a chain you plan: ordered, with
//! named roles. This models the assertion "that payment led to this one", made between any two
//! transactions after the fact. There is no container to create first and no sequence to get
//! right, which is the whole point -- you link what you notice, when you notice it.
//!
//! Links are DIRECTED so a chain can be drawn with arrows, and TRAVERSED UNDIRECTED so it does not
//! matter which end you start from. That asymmetry is deliberate: direction is information worth
//! keeping, but it must never determine what you can reach.

use crate::money::Minor;
use rusqlite::Connection;
use std::collections::{HashSet, VecDeque};

#[derive(Debug)]
pub enum LinkError {
    Sql(rusqlite::Error),
    NoSuchTxn(i64),
    SelfLink,
    Exists(i64, i64),
    Unexpected(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Sql(e) => write!(f, "{e}"),
            LinkError::NoSuchTxn(id) => write!(f, "no such transaction: {id}"),
            LinkError::SelfLink => write!(f, "a payment cannot link to itself"),
            LinkError::Exists(a, b) => write!(f, "{a} is already linked to {b}"),
            LinkError::Unexpected(m) => write!(f, "{m}"),
        }
    }
}

impl From<rusqlite::Error> for LinkError {
    fn from(e: rusqlite::Error) -> Self {
        LinkError::Sql(e)
    }
}

// The graph borrows entry::summarise so a payment reads identically in a list and in a node.
// summarise only ever READS, so every error it can raise is a SQL one; the catch-all keeps the
// message rather than inventing a plausible-looking substitute, so a surprise stays legible.
impl From<crate::entry::EntryError> for LinkError {
    fn from(e: crate::entry::EntryError) -> Self {
        match e {
            crate::entry::EntryError::Sql(inner) => LinkError::Sql(inner),
            other => LinkError::Unexpected(other.to_string()),
        }
    }
}

fn exists(conn: &Connection, id: i64) -> Result<bool, LinkError> {
    Ok(conn.query_row("SELECT 1 FROM txn WHERE id = ?1", [id], |_| Ok(())).is_ok())
}

pub fn create(
    conn: &Connection,
    from: i64,
    to: i64,
    note: Option<&str>,
    on: &str,
) -> Result<(), LinkError> {
    if from == to {
        return Err(LinkError::SelfLink);
    }
    for id in [from, to] {
        if !exists(conn, id)? {
            return Err(LinkError::NoSuchTxn(id));
        }
    }
    // An existing edge in EITHER direction is the same assertion, so re-adding it reversed is a
    // duplicate rather than a new fact. Reported rather than silently ignored: the operator asked
    // for something the graph already says.
    let already: bool = conn.query_row(
        "SELECT 1 FROM txn_link
          WHERE (from_txn_id = ?1 AND to_txn_id = ?2)
             OR (from_txn_id = ?2 AND to_txn_id = ?1)",
        [from, to],
        |_| Ok(()),
    )
    .is_ok();
    if already {
        return Err(LinkError::Exists(from, to));
    }
    conn.execute(
        "INSERT INTO txn_link(from_txn_id, to_txn_id, note, created_on) VALUES(?1,?2,?3,?4)",
        rusqlite::params![from, to, note, on],
    )?;
    Ok(())
}

/// Removes the edge in whichever direction it was recorded -- the caller should not have to know.
pub fn remove(conn: &Connection, a: i64, b: i64) -> Result<bool, LinkError> {
    let n = conn.execute(
        "DELETE FROM txn_link
          WHERE (from_txn_id = ?1 AND to_txn_id = ?2)
             OR (from_txn_id = ?2 AND to_txn_id = ?1)",
        [a, b],
    )?;
    Ok(n > 0)
}

/// Everything directly linked to one transaction, in both directions.
pub fn for_txn(conn: &Connection, txn_id: i64) -> Result<serde_json::Value, LinkError> {
    let mut st = conn.prepare(
        "SELECT to_txn_id, 'out', note FROM txn_link WHERE from_txn_id = ?1
         UNION ALL
         SELECT from_txn_id, 'in', note FROM txn_link WHERE to_txn_id = ?1",
    )?;
    let edges: Vec<(i64, String, Option<String>)> = st
        .query_map([txn_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    let ids: Vec<i64> = edges.iter().map(|(id, _, _)| *id).collect();
    let mut summaries = crate::entry::summarise(conn, &ids)?;
    for s in &mut summaries {
        let id = s["id"].as_i64().unwrap_or(0);
        if let Some((_, dir, note)) = edges.iter().find(|(e, _, _)| *e == id) {
            s["direction"] = serde_json::json!(dir);
            s["note"] = serde_json::json!(note);
        }
    }
    Ok(serde_json::json!({ "txn_id": txn_id, "linked": summaries }))
}

/// The whole connected component containing `txn_id` -- the chain, however it was built.
///
/// Breadth-first and undirected, so starting anywhere in a chain yields the same set. `depth` on
/// each node is hops from the starting transaction, which is what lets a view lay the graph out
/// without computing its own distances.
pub fn chain(conn: &Connection, txn_id: i64, max_nodes: usize) -> Result<serde_json::Value, LinkError> {
    if !exists(conn, txn_id)? {
        return Err(LinkError::NoSuchTxn(txn_id));
    }
    let mut seen: HashSet<i64> = HashSet::new();
    let mut depth: Vec<(i64, i64)> = Vec::new();
    let mut queue: VecDeque<(i64, i64)> = VecDeque::new();
    queue.push_back((txn_id, 0));
    seen.insert(txn_id);

    let mut st = conn.prepare(
        "SELECT to_txn_id FROM txn_link WHERE from_txn_id = ?1
         UNION
         SELECT from_txn_id FROM txn_link WHERE to_txn_id = ?1",
    )?;
    let mut truncated = false;
    while let Some((id, d)) = queue.pop_front() {
        depth.push((id, d));
        if seen.len() >= max_nodes {
            truncated = true;
            continue;
        }
        let next: Vec<i64> =
            st.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?;
        for n in next {
            if seen.insert(n) {
                queue.push_back((n, d + 1));
            }
        }
    }

    let ids: Vec<i64> = depth.iter().map(|(id, _)| *id).collect();
    let mut nodes = crate::entry::summarise(conn, &ids)?;
    for n in &mut nodes {
        let id = n["id"].as_i64().unwrap_or(0);
        n["depth"] = serde_json::json!(depth.iter().find(|(i, _)| *i == id).map(|(_, d)| *d));
        n["is_root"] = serde_json::json!(id == txn_id);
    }

    // Only edges BETWEEN nodes we returned: an edge to something outside the set would draw an
    // arrow to nothing.
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let edges: Vec<serde_json::Value> = if ids.is_empty() {
        Vec::new()
    } else {
        let mut st = conn.prepare(&format!(
            "SELECT from_txn_id, to_txn_id, note FROM txn_link
              WHERE from_txn_id IN ({list}) AND to_txn_id IN ({list})"
        ))?;
        // Bound before the block ends: `st` borrows `conn` and the iterator borrows `st`, so
        // the collect has to finish while both are still alive.
        let collected: Vec<serde_json::Value> = st
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "from": r.get::<_, i64>(0)?,
                    "to": r.get::<_, i64>(1)?,
                    "note": r.get::<_, Option<String>>(2)?,
                }))
            })?
            .collect::<Result<_, _>>()?;
        collected
    };

    // Net movement per account across the whole thread. This is the question a chain exists to
    // answer -- "where did this actually end up" -- and it is NOT a completion test: a finished
    // transfer still shows its source, its destination and any fee. It is the accounts money
    // passed straight THROUGH that fall to zero and drop out, which is the useful signal.
    let residual: Vec<serde_json::Value> = if ids.is_empty() {
        Vec::new()
    } else {
        let mut st = conn.prepare(&format!(
            "SELECT a.name, a.kind, p.currency, SUM(p.amount_minor)
               FROM posting p JOIN account a ON a.id = p.account_id
              WHERE p.txn_id IN ({list})
              GROUP BY a.id, p.currency
             HAVING SUM(p.amount_minor) <> 0
              ORDER BY SUM(p.amount_minor)"
        ))?;
        let collected: Vec<serde_json::Value> = st
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "account": r.get::<_, String>(0)?,
                    "kind": r.get::<_, String>(1)?,
                    "currency": r.get::<_, String>(2)?,
                    "amount_minor": r.get::<_, Minor>(3)?,
                }))
            })?
            .collect::<Result<_, _>>()?;
        collected
    };

    Ok(serde_json::json!({
        "root": txn_id,
        "nodes": nodes,
        "edges": edges,
        "residual": residual,
        "truncated": truncated,
    }))
}
