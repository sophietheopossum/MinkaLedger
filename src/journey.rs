//! Journeys: one movement of money, many hops (requirements 5 and 10).
//!
//! A transfer between two accounts needs no journey -- double-entry already ties both sides of a
//! single transaction together, which is requirement 5's easy half. A journey exists for the case
//! double-entry cannot express on its own: money that takes SEVERAL transactions to arrive.
//! Current -> PayPal -> a merchant, or a Wise transfer where the GBP leaves on Monday and the EUR
//! lands on Wednesday. Each hop is its own transaction with its own date; the journey is what says
//! they are one story.
//!
//! THE RESIDUAL is the point. At any moment a journey's legs may not have arrived yet, and the
//! difference between what has left the source and what has reached the terminus is money in
//! flight. If it is non-zero after the journey closes, something went missing -- a fee you did not
//! record, or a leg you forgot to attach.

use rusqlite::Connection;

use crate::money::Minor;

#[derive(Debug)]
pub enum JourneyError {
    Sql(rusqlite::Error),
    NotFound(i64),
    NoSuchTxn(i64),
}

impl std::fmt::Display for JourneyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JourneyError::Sql(e) => write!(f, "{e}"),
            JourneyError::NotFound(id) => write!(f, "no such journey: {id}"),
            JourneyError::NoSuchTxn(id) => write!(f, "no such transaction: {id}"),
        }
    }
}

impl From<rusqlite::Error> for JourneyError {
    fn from(e: rusqlite::Error) -> Self {
        JourneyError::Sql(e)
    }
}

pub fn create(conn: &Connection, label: &str, opened_on: &str) -> Result<i64, JourneyError> {
    conn.execute(
        "INSERT INTO journey(label, opened_on) VALUES(?1, ?2)",
        rusqlite::params![label, opened_on],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Attach a transaction as a hop. `seq` orders the legs; `role` marks which end it is.
pub fn attach(
    conn: &Connection,
    journey_id: i64,
    txn_id: i64,
    seq: i64,
    role: &str,
) -> Result<(), JourneyError> {
    let exists: i64 =
        conn.query_row("SELECT COUNT(*) FROM journey WHERE id = ?1", [journey_id], |r| r.get(0))?;
    if exists == 0 {
        return Err(JourneyError::NotFound(journey_id));
    }
    let exists: i64 =
        conn.query_row("SELECT COUNT(*) FROM txn WHERE id = ?1", [txn_id], |r| r.get(0))?;
    if exists == 0 {
        return Err(JourneyError::NoSuchTxn(txn_id));
    }
    conn.execute(
        "INSERT INTO journey_member(journey_id, txn_id, seq, role) VALUES(?1,?2,?3,?4)
         ON CONFLICT(journey_id, txn_id) DO UPDATE SET seq = excluded.seq, role = excluded.role",
        rusqlite::params![journey_id, txn_id, seq, role],
    )?;
    Ok(())
}

pub fn detach(conn: &Connection, journey_id: i64, txn_id: i64) -> Result<bool, JourneyError> {
    let n = conn.execute(
        "DELETE FROM journey_member WHERE journey_id = ?1 AND txn_id = ?2",
        rusqlite::params![journey_id, txn_id],
    )?;
    Ok(n > 0)
}

pub fn close(conn: &Connection, journey_id: i64, on: &str) -> Result<(), JourneyError> {
    let n = conn.execute(
        "UPDATE journey SET closed_on = ?2 WHERE id = ?1",
        rusqlite::params![journey_id, on],
    )?;
    if n == 0 {
        return Err(JourneyError::NotFound(journey_id));
    }
    Ok(())
}

/// The whole story of one journey: its legs in order, and what is still in flight.
///
/// The residual is computed per currency over the accounts the journey PASSES THROUGH -- the
/// intermediate holding accounts. Money that has left the source and reached the terminus nets to
/// zero across those; money still sitting in PayPal does not.
pub fn get(conn: &Connection, journey_id: i64) -> Result<serde_json::Value, JourneyError> {
    let mut head = conn
        .query_row(
            "SELECT id, label, opened_on, closed_on FROM journey WHERE id = ?1",
            [journey_id],
            |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "label": r.get::<_, String>(1)?,
                    "opened_on": r.get::<_, String>(2)?,
                    "closed_on": r.get::<_, Option<String>>(3)?,
                }))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => JourneyError::NotFound(journey_id),
            other => JourneyError::Sql(other),
        })?;

    let mut stmt = conn.prepare(
        "SELECT m.seq, m.role, t.id, t.occurred_on, t.description
           FROM journey_member m JOIN txn t ON t.id = m.txn_id
          WHERE m.journey_id = ?1
          ORDER BY m.seq, t.occurred_on, t.id",
    )?;
    let legs: Vec<serde_json::Value> = stmt
        .query_map([journey_id], |r| {
            Ok(serde_json::json!({
                "seq": r.get::<_, i64>(0)?,
                "role": r.get::<_, String>(1)?,
                "txn_id": r.get::<_, i64>(2)?,
                "occurred_on": r.get::<_, String>(3)?,
                "description": r.get::<_, String>(4)?,
            }))
        })?
        .collect::<Result<_, _>>()?;

    // Net movement per account across every leg. A completed journey leaves its intermediate
    // accounts at zero; whatever remains is in flight (or a fee that was never recorded).
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, p.currency, SUM(p.amount_minor)
           FROM journey_member m
           JOIN posting p ON p.txn_id = m.txn_id
           JOIN account a ON a.id = p.account_id
          WHERE m.journey_id = ?1
          GROUP BY a.id, p.currency
         HAVING SUM(p.amount_minor) <> 0
          ORDER BY a.name",
    )?;
    let residual: Vec<serde_json::Value> = stmt
        .query_map([journey_id], |r| {
            Ok(serde_json::json!({
                "account_id": r.get::<_, i64>(0)?,
                "account": r.get::<_, String>(1)?,
                "currency": r.get::<_, String>(2)?,
                "amount_minor": r.get::<_, Minor>(3)?,
            }))
        })?
        .collect::<Result<_, _>>()?;

    head["legs"] = serde_json::Value::Array(legs);
    head["residual"] = serde_json::Value::Array(residual);
    Ok(head)
}

pub fn list(conn: &Connection) -> Result<Vec<serde_json::Value>, JourneyError> {
    let mut stmt = conn.prepare(
        "SELECT j.id, j.label, j.opened_on, j.closed_on,
                (SELECT COUNT(*) FROM journey_member m WHERE m.journey_id = j.id)
           FROM journey j ORDER BY j.opened_on DESC, j.id DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "label": r.get::<_, String>(1)?,
                "opened_on": r.get::<_, String>(2)?,
                "closed_on": r.get::<_, Option<String>>(3)?,
                "legs": r.get::<_, i64>(4)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every journey a transaction belongs to. A txn may be part of more than one.
pub fn for_txn(conn: &Connection, txn_id: i64) -> Result<Vec<serde_json::Value>, JourneyError> {
    let mut stmt = conn.prepare(
        "SELECT j.id, j.label, m.seq, m.role
           FROM journey_member m JOIN journey j ON j.id = m.journey_id
          WHERE m.txn_id = ?1 ORDER BY j.id",
    )?;
    let rows = stmt
        .query_map([txn_id], |r| {
            Ok(serde_json::json!({
                "journey_id": r.get::<_, i64>(0)?,
                "label": r.get::<_, String>(1)?,
                "seq": r.get::<_, i64>(2)?,
                "role": r.get::<_, String>(3)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{self, NewPosting, NewTxn};

    fn book() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql")).unwrap();
        conn.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES
               (1,'Current','asset','GBP'),
               (2,'PayPal','asset','GBP'),
               (3,'Merchant','expense','GBP'),
               (4,'Fees','expense','GBP');",
        )
        .unwrap();
        conn
    }

    fn pay(conn: &mut Connection, on: &str, desc: &str, legs: &[(i64, i64)]) -> i64 {
        entry::create(
            conn,
            &NewTxn {
                occurred_on: on.into(),
                description: desc.into(),
                payee: None,
                note: None,
                postings: legs
                    .iter()
                    .map(|(a, m)| NewPosting { account_id: *a, amount_minor: *m })
                    .collect(),
                series_id: None,
                occurrence_on: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_completed_three_hop_journey_has_no_residual() {
        let mut c = book();
        let j = create(&c, "Concert tickets via PayPal", "2026-08-01").unwrap();
        // £100 Current -> PayPal, then PayPal -> merchant.
        let a = pay(&mut c, "2026-08-01", "top up PayPal", &[(1, -10_000), (2, 10_000)]);
        let b = pay(&mut c, "2026-08-03", "tickets", &[(2, -10_000), (3, 10_000)]);
        attach(&c, j, a, 1, "source").unwrap();
        attach(&c, j, b, 2, "arrival").unwrap();

        let got = get(&c, j).unwrap();
        assert_eq!(got["legs"].as_array().unwrap().len(), 2);
        // PayPal nets to zero -- the money passed through and arrived.
        let residual = got["residual"].as_array().unwrap();
        let paypal = residual.iter().find(|r| r["account"] == "PayPal");
        assert!(paypal.is_none(), "PayPal should net to zero: {residual:?}");
    }

    #[test]
    fn money_still_in_flight_shows_as_residual() {
        let mut c = book();
        let j = create(&c, "Transfer in progress", "2026-08-01").unwrap();
        // left the current account, sitting in PayPal, not yet spent
        let a = pay(&mut c, "2026-08-01", "top up PayPal", &[(1, -10_000), (2, 10_000)]);
        attach(&c, j, a, 1, "source").unwrap();

        let got = get(&c, j).unwrap();
        let residual = got["residual"].as_array().unwrap();
        let paypal = residual.iter().find(|r| r["account"] == "PayPal").expect("in flight");
        assert_eq!(paypal["amount_minor"], 10_000, "£100 is sitting in PayPal");
    }

    #[test]
    fn a_fee_leg_is_part_of_the_story() {
        let mut c = book();
        let j = create(&c, "Wise transfer", "2026-08-01").unwrap();
        // £100 leaves, £3 fee, £97 arrives -- three accounts, one journey.
        let a = pay(&mut c, "2026-08-01", "send", &[(1, -10_000), (2, 9_700), (4, 300)]);
        let b = pay(&mut c, "2026-08-02", "spend", &[(2, -9_700), (3, 9_700)]);
        attach(&c, j, a, 1, "source").unwrap();
        attach(&c, j, b, 2, "arrival").unwrap();
        let got = get(&c, j).unwrap();
        let residual = got["residual"].as_array().unwrap();
        // the holding account is clear; the fee is a real expense and stays visible
        assert!(residual.iter().all(|r| r["account"] != "PayPal"));
        let fee = residual.iter().find(|r| r["account"] == "Fees").unwrap();
        assert_eq!(fee["amount_minor"], 300);
    }

    #[test]
    fn legs_come_back_in_sequence_order() {
        let mut c = book();
        let j = create(&c, "j", "2026-08-01").unwrap();
        let a = pay(&mut c, "2026-08-05", "second", &[(2, -100), (3, 100)]);
        let b = pay(&mut c, "2026-08-01", "first", &[(1, -100), (2, 100)]);
        attach(&c, j, a, 2, "arrival").unwrap();
        attach(&c, j, b, 1, "source").unwrap();
        let got = get(&c, j).unwrap();
        let legs = got["legs"].as_array().unwrap();
        assert_eq!(legs[0]["description"], "first");
        assert_eq!(legs[1]["description"], "second");
    }

    #[test]
    fn a_txn_can_belong_to_several_journeys() {
        let mut c = book();
        let j1 = create(&c, "one", "2026-08-01").unwrap();
        let j2 = create(&c, "two", "2026-08-01").unwrap();
        let t = pay(&mut c, "2026-08-01", "shared", &[(1, -100), (2, 100)]);
        attach(&c, j1, t, 1, "leg").unwrap();
        attach(&c, j2, t, 1, "leg").unwrap();
        assert_eq!(for_txn(&c, t).unwrap().len(), 2);
    }

    #[test]
    fn attaching_twice_updates_rather_than_duplicating() {
        let mut c = book();
        let j = create(&c, "j", "2026-08-01").unwrap();
        let t = pay(&mut c, "2026-08-01", "x", &[(1, -100), (2, 100)]);
        attach(&c, j, t, 1, "leg").unwrap();
        attach(&c, j, t, 5, "arrival").unwrap();
        let got = get(&c, j).unwrap();
        let legs = got["legs"].as_array().unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0]["seq"], 5);
        assert_eq!(legs[0]["role"], "arrival");
    }

    #[test]
    fn detach_and_missing_ids_behave() {
        let mut c = book();
        let j = create(&c, "j", "2026-08-01").unwrap();
        let t = pay(&mut c, "2026-08-01", "x", &[(1, -100), (2, 100)]);
        attach(&c, j, t, 1, "leg").unwrap();
        assert!(detach(&c, j, t).unwrap());
        assert!(!detach(&c, j, t).unwrap());
        assert!(matches!(attach(&c, 404, t, 1, "leg"), Err(JourneyError::NotFound(404))));
        assert!(matches!(attach(&c, j, 404, 1, "leg"), Err(JourneyError::NoSuchTxn(404))));
        assert!(matches!(get(&c, 404), Err(JourneyError::NotFound(404))));
    }

    #[test]
    fn deleting_a_txn_removes_it_from_its_journeys() {
        let mut c = book();
        let j = create(&c, "j", "2026-08-01").unwrap();
        let t = pay(&mut c, "2026-08-01", "x", &[(1, -100), (2, 100)]);
        attach(&c, j, t, 1, "leg").unwrap();
        entry::delete(&c, t).unwrap();
        // ON DELETE CASCADE on journey_member.txn_id -- no dangling membership
        assert_eq!(get(&c, j).unwrap()["legs"].as_array().unwrap().len(), 0);
    }
}
