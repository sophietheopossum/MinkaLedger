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
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

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

/// Which node a payment's leg lands on, before links merge any of them.
///
/// Income, expense and equity accounts are SHARED: every salary diverges from the one Salary
/// node and every shop merges into the one Groceries node, because those are where a chain
/// begins and ends and there is nothing to tell one visit apart from the next. Everything else --
/// the places money sits on the way -- is a node PER PAYMENT until a link says two payments were
/// the same visit.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum NodeKey {
    Shared(i64),
    Own(i64, i64), // (txn, account)
}

fn shared_kind(kind: &str) -> bool {
    matches!(kind, "income" | "expense" | "equity")
}

/// Every payment in a window as a graph of ACCOUNT VISITS: each payment is an edge from the
/// account its money left to the account it arrived at, and a node is one visit to an account.
///
/// WHY VISITS AND NOT ACCOUNTS. Drawn account-to-account, every payment from Current to
/// Groceries would collapse onto one arrow and the picture would be the chart of accounts. Drawn
/// payment-to-payment, a chain is a line of boxes and the account the money passed through is a
/// label rather than a place. Visits give both: an unlinked payment is two nodes and an arrow,
/// and a chain is the arrows joined end to end, because a link between two payments that touch
/// the same account says the arrival and the departure were ONE visit and the two nodes become
/// one. Two chains that converge on the same visit share that node, which then carries every
/// arrow in and out and is drawn as large as its traffic.
///
/// A link between payments that touch no account in common cannot be a shared visit, so it is
/// returned separately as a `links` entry from the earlier payment's arrival node to the later
/// one's departure node, for a view to draw as an assertion rather than as money moving.
///
/// Only real legs count: a conversion's own legs sit in the conversion accounts and only make
/// each currency balance, so a conversion is one arrow whose two ends are in different currencies.
/// A split fans out into one arrow per arriving leg (or per leaving leg, for a merge), each
/// carrying that leg's amount.
pub fn graph(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
    limit: i64,
) -> Result<serde_json::Value, LinkError> {
    let mut st = conn.prepare(
        "SELECT id FROM txn
          WHERE (?1 IS NULL OR occurred_on >= ?1)
            AND (?2 IS NULL OR occurred_on <= ?2)
          ORDER BY occurred_on DESC, id DESC
          LIMIT ?3",
    )?;
    let ids: Vec<i64> = st
        .query_map(rusqlite::params![from, to, limit], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM txn
          WHERE (?1 IS NULL OR occurred_on >= ?1)
            AND (?2 IS NULL OR occurred_on <= ?2)",
        rusqlite::params![from, to],
        |r| r.get(0),
    )?;
    let payments = crate::entry::summarise(conn, &ids)?;

    // A payment's real legs, as (account_id, name, kind, currency, amount), leaving first.
    type Leg = (i64, String, String, String, Minor);
    let legs_of = |p: &serde_json::Value| -> Vec<Leg> {
        let mut legs: Vec<Leg> = p["postings"]
            .as_array()
            .map(|a| a.iter())
            .into_iter()
            .flatten()
            .filter(|l| l["kind"] != "conversion")
            .map(|l| {
                (
                    l["account_id"].as_i64().unwrap_or(0),
                    l["account"].as_str().unwrap_or("").to_string(),
                    l["kind"].as_str().unwrap_or("").to_string(),
                    l["currency"].as_str().unwrap_or("").to_string(),
                    l["amount_minor"].as_i64().unwrap_or(0),
                )
            })
            .collect();
        legs.sort_by_key(|l| l.4);
        legs
    };
    let key_of = |txn: i64, leg: &Leg| -> NodeKey {
        if shared_kind(&leg.2) { NodeKey::Shared(leg.0) } else { NodeKey::Own(txn, leg.0) }
    };

    // One slot per distinct key, then union-find over them as links merge visits.
    let mut slot: HashMap<NodeKey, usize> = HashMap::new();
    let mut info: Vec<(i64, String, String, String)> = Vec::new(); // account, name, kind, currency
    let mut parent: Vec<usize> = Vec::new();
    let mut legs_by_txn: HashMap<i64, Vec<Leg>> = HashMap::new();
    for p in &payments {
        let txn = p["id"].as_i64().unwrap_or(0);
        let legs = legs_of(p);
        for leg in &legs {
            let key = key_of(txn, leg);
            slot.entry(key).or_insert_with(|| {
                info.push((leg.0, leg.1.clone(), leg.2.clone(), leg.3.clone()));
                parent.push(parent.len());
                parent.len() - 1
            });
        }
        legs_by_txn.insert(txn, legs);
    }
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }

    // The links among these payments. A link between payments sharing an account merges that
    // account's visit; one sharing nothing is kept to draw on its own.
    let links: Vec<(i64, i64, Option<String>)> = if ids.is_empty() {
        Vec::new()
    } else {
        let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let mut st = conn.prepare(&format!(
            "SELECT from_txn_id, to_txn_id, note FROM txn_link
              WHERE from_txn_id IN ({list}) AND to_txn_id IN ({list})"
        ))?;
        let collected: Vec<(i64, i64, Option<String>)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        collected
    };
    let mut loose: Vec<(i64, i64, Option<String>)> = Vec::new();
    for (a, b, note) in &links {
        let (Some(la), Some(lb)) = (legs_by_txn.get(a), legs_by_txn.get(b)) else { continue };
        let mut shared_any = false;
        for x in la {
            if let Some(y) = lb.iter().find(|y| y.0 == x.0) {
                shared_any = true;
                if !shared_kind(&x.2) {
                    let i = slot[&key_of(*a, x)];
                    let j = slot[&key_of(*b, y)];
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    if ri != rj {
                        parent[ri] = rj;
                    }
                }
            }
        }
        if !shared_any {
            loose.push((*a, *b, note.clone()));
        }
    }

    // Compact the roots into output ids, in first-seen order so a view's layout is stable
    // between two identical answers.
    let mut out_id: HashMap<usize, usize> = HashMap::new();
    let mut nodes: Vec<serde_json::Value> = Vec::new();
    let mut members: Vec<BTreeSet<i64>> = Vec::new();
    let mut degree: Vec<(i64, i64)> = Vec::new(); // (in, out)
    let node_of = |slot_ix: usize,
                   parent: &mut Vec<usize>,
                   out_id: &mut HashMap<usize, usize>,
                   nodes: &mut Vec<serde_json::Value>,
                   members: &mut Vec<BTreeSet<i64>>,
                   degree: &mut Vec<(i64, i64)>|
     -> usize {
        let root = find(parent, slot_ix);
        *out_id.entry(root).or_insert_with(|| {
            let (account_id, name, kind, currency) = &info[root];
            nodes.push(serde_json::json!({
                "id": nodes.len(),
                "account_id": account_id,
                "account": name,
                "kind": kind,
                "currency": currency,
                "shared": shared_kind(kind),
            }));
            members.push(BTreeSet::new());
            degree.push((0, 0));
            nodes.len() - 1
        })
    };

    let mut edges: Vec<serde_json::Value> = Vec::new();
    let mut arrival: HashMap<i64, usize> = HashMap::new();
    let mut departure: HashMap<i64, usize> = HashMap::new();
    for p in &payments {
        let txn = p["id"].as_i64().unwrap_or(0);
        let legs = &legs_by_txn[&txn];
        let leaving: Vec<&Leg> = legs.iter().filter(|l| l.4 < 0).collect();
        let arriving: Vec<&Leg> = legs.iter().filter(|l| l.4 > 0).collect();
        for l in &leaving {
            let n = node_of(slot[&key_of(txn, l)], &mut parent, &mut out_id, &mut nodes, &mut members, &mut degree);
            members[n].insert(txn);
            departure.entry(txn).or_insert(n);
        }
        for a in &arriving {
            let n = node_of(slot[&key_of(txn, a)], &mut parent, &mut out_id, &mut nodes, &mut members, &mut degree);
            members[n].insert(txn);
            arrival.entry(txn).or_insert(n);
        }
        for l in &leaving {
            for a in &arriving {
                let fi = node_of(slot[&key_of(txn, l)], &mut parent, &mut out_id, &mut nodes, &mut members, &mut degree);
                let ti = node_of(slot[&key_of(txn, a)], &mut parent, &mut out_id, &mut nodes, &mut members, &mut degree);
                degree[fi].1 += 1;
                degree[ti].0 += 1;
                // The arrow carries what moved along it: for a single payment or a conversion
                // that is the leg itself, for a split the smaller of the two ends it joins.
                let mut edge = serde_json::json!({
                    "txn_id": txn,
                    "from": fi,
                    "to": ti,
                    "occurred_on": p["occurred_on"],
                    "description": p["description"],
                    "amount_minor": (-l.4).min(a.4),
                    "currency": l.3,
                    "links": p["links"],
                });
                if l.3 != a.3 {
                    edge["to_currency"] = serde_json::json!(a.3);
                    edge["to_minor"] = serde_json::json!(a.4);
                }
                edges.push(edge);
            }
        }
    }
    for (i, n) in nodes.iter_mut().enumerate() {
        n["txn_ids"] = serde_json::json!(members[i].iter().collect::<Vec<_>>());
        n["in"] = serde_json::json!(degree[i].0);
        n["out"] = serde_json::json!(degree[i].1);
    }
    // Every link, so a view can offer to undo the ones a merged node stands for; the loose ones
    // additionally say which nodes to draw them between.
    let link_rows: Vec<serde_json::Value> = links
        .iter()
        .map(|(a, b, note)| {
            let mut row = serde_json::json!({ "from_txn": a, "to_txn": b, "note": note, "shared": true });
            if let Some((_, _, _)) = loose.iter().find(|(x, y, _)| x == a && y == b) {
                row["shared"] = serde_json::json!(false);
                row["from_node"] = serde_json::json!(arrival.get(a).or(departure.get(a)));
                row["to_node"] = serde_json::json!(departure.get(b).or(arrival.get(b)));
            }
            row
        })
        .collect();

    Ok(serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "links": link_rows,
        "payments": ids.len(),
        "total": total,
    }))
}
