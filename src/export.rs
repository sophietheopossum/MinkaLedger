//! Export for analysis (requirement 7).
//!
//! The audience is a language model reading this cold, so the export leads with a PREAMBLE stating
//! the invariants it must not violate. Those three lines are the whole feature: without them a
//! model reliably sums across currencies, divides minor units by 100 when the currency has 0 or 8
//! decimal places, and counts postings as if they were transactions -- producing a confident,
//! wrong answer. The data was always exportable; what makes it *usable* is saying what it means.
//!
//! Projections are included only on request and are flagged `is_projection: true` on every row.
//! Mixing a forecast into history unlabelled is the one mistake that would make an analysis
//! actively misleading rather than merely wrong.

use rusqlite::Connection;

#[derive(Debug)]
pub enum ExportError {
    Sql(rusqlite::Error),
    Io(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Sql(e) => write!(f, "{e}"),
            ExportError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl From<rusqlite::Error> for ExportError {
    fn from(e: rusqlite::Error) -> Self {
        ExportError::Sql(e)
    }
}

/// What the reader has to know to not draw a wrong conclusion. Deliberately blunt, and deliberately
/// about the traps rather than about the schema -- the schema is self-describing, the traps are not.
pub const PREAMBLE: &[&str] = &[
    "This is a double-entry ledger. Each transaction (txn_id) has two or more postings that SUM TO ZERO within each currency. Counting postings will double-count; group by txn_id.",
    "amount_minor is an INTEGER in the currency's minor unit. The divisor is 10^minor_digits, which is NOT always 100: JPY has 0, BTC has 8. Use amount_decimal if you want a ready-made decimal.",
    "Amounts in different currencies are NOT comparable and must never be summed together. Group by currency, or convert explicitly using a dated rate.",
    "Sign convention: positive increases the account, negative decreases it. An expense posting is positive; the asset it was paid from is negative. Income accounts hold negative balances.",
    "Rows with is_projection=true are FORECAST, not history. They have not happened. Never mix them into a total of what was actually spent.",
];

fn facts(conn: &Connection) -> Result<Vec<serde_json::Value>, ExportError> {
    let mut st = conn.prepare(
        "SELECT account, account_kind, currency, posting_count, first_on, last_on, closing_minor
           FROM v_export_facts ORDER BY account_kind, account",
    )?;
    let out = st
        .query_map([], |r| {
            Ok(serde_json::json!({
                "account": r.get::<_, String>(0)?,
                "account_kind": r.get::<_, String>(1)?,
                "currency": r.get::<_, String>(2)?,
                "posting_count": r.get::<_, i64>(3)?,
                "first_on": r.get::<_, Option<String>>(4)?,
                "last_on": r.get::<_, Option<String>>(5)?,
                "closing_minor": r.get::<_, i64>(6)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}

/// Every ledger line, as JSON objects. `redact` blanks the free-text fields, which is what makes it
/// safe to hand a book to a model you do not control: the shape and the numbers survive, the
/// payees and descriptions do not.
fn lines(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
    redact: bool,
) -> Result<Vec<serde_json::Value>, ExportError> {
    let mut st = conn.prepare(
        "SELECT posting_id, txn_id, on_date, account, account_kind, description, payee,
                amount_minor, currency, minor_digits, amount_decimal, series_description,
                occurrence_on, source
           FROM v_ledger_line
          WHERE (?1 IS NULL OR on_date >= ?1) AND (?2 IS NULL OR on_date <= ?2)
          ORDER BY on_date, txn_id, posting_id",
    )?;
    let out = st
        .query_map(rusqlite::params![from, to], |r| {
            let desc: String = r.get(5)?;
            let payee: Option<String> = r.get(6)?;
            Ok(serde_json::json!({
                "posting_id": r.get::<_, i64>(0)?,
                "txn_id": r.get::<_, i64>(1)?,
                "on_date": r.get::<_, String>(2)?,
                "account": r.get::<_, String>(3)?,
                "account_kind": r.get::<_, String>(4)?,
                "description": if redact { "[redacted]".to_string() } else { desc },
                "payee": if redact { None } else { payee },
                "amount_minor": r.get::<_, i64>(7)?,
                "currency": r.get::<_, String>(8)?,
                "minor_digits": r.get::<_, i64>(9)?,
                "amount_decimal": r.get::<_, Option<String>>(10)?,
                "series": r.get::<_, Option<String>>(11)?,
                "occurrence_on": r.get::<_, Option<String>>(12)?,
                "source": r.get::<_, String>(13)?,
                "is_projection": false,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}

pub struct Options<'a> {
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub redact: bool,
    /// Append projected occurrences, each flagged `is_projection: true`.
    pub forecast_to: Option<&'a str>,
}

/// The whole export as one JSON document: preamble, facts, then lines.
pub fn bundle(conn: &Connection, opt: &Options) -> Result<serde_json::Value, ExportError> {
    let mut rows = lines(conn, opt.from, opt.to, opt.redact)?;

    if let Some(horizon) = opt.forecast_to {
        let as_of = opt
            .to
            .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .unwrap_or_else(|| chrono::Local::now().date_naive());
        if let Ok(h) = chrono::NaiveDate::parse_from_str(horizon, "%Y-%m-%d") {
            let snap = crate::forecast::load::snapshot(conn, as_of)?;
            if let Ok(proj) = crate::forecast::project(
                &crate::recur::RRuleCrate,
                &snap,
                as_of,
                h,
                &std::collections::HashSet::new(),
            ) {
                for o in proj.occurrences {
                    // A transaction dated after as_of is replayed by the projection so the
                    // balances are right, but it is already among the ledger lines above with
                    // its own id: listing it again as a projection would count it twice.
                    if o.txn_id.is_some() {
                        continue;
                    }
                    rows.push(serde_json::json!({
                        "posting_id": serde_json::Value::Null,
                        "txn_id": serde_json::Value::Null,
                        "on_date": o.value_on,
                        "account": o.account,
                        "account_kind": serde_json::Value::Null,
                        "description": if opt.redact { "[redacted]".into() } else { o.description },
                        "payee": serde_json::Value::Null,
                        "amount_minor": o.amount_minor,
                        "currency": o.currency,
                        "minor_digits": serde_json::Value::Null,
                        "amount_decimal": serde_json::Value::Null,
                        "series": serde_json::Value::Null,
                        "occurrence_on": o.occurrence_on,
                        "source": "forecast",
                        "is_projection": true,
                    }));
                }
            }
        }
    }

    Ok(serde_json::json!({
        "format": "minka-ledger export v1",
        "generated_on": chrono::Local::now().date_naive().to_string(),
        "read_this_first": PREAMBLE,
        "currencies": currencies(conn)?,
        "account_summary": facts(conn)?,
        "lines": rows,
    }))
}

fn currencies(conn: &Connection) -> Result<Vec<serde_json::Value>, ExportError> {
    let mut st = conn.prepare("SELECT code, minor_digits, name FROM currency ORDER BY code")?;
    let out = st
        .query_map([], |r| {
            Ok(serde_json::json!({
                "code": r.get::<_, String>(0)?,
                "minor_digits": r.get::<_, i64>(1)?,
                "name": r.get::<_, String>(2)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}

/// Flat CSV of the ledger lines, for a spreadsheet. The preamble cannot travel with it, so the
/// caller gets it separately -- which is precisely why the JSON bundle is the recommended shape.
pub fn csv(conn: &Connection, opt: &Options) -> Result<String, ExportError> {
    let rows = lines(conn, opt.from, opt.to, opt.redact)?;
    let mut out = String::from(
        "on_date,txn_id,account,account_kind,description,payee,amount_minor,amount_decimal,currency,minor_digits,source\n",
    );
    let esc = |v: &serde_json::Value| -> String {
        let s = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        if s.contains([',', '"', '\n']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s
        }
    };
    for r in rows {
        let cols = [
            "on_date", "txn_id", "account", "account_kind", "description", "payee",
            "amount_minor", "amount_decimal", "currency", "minor_digits", "source",
        ];
        let line: Vec<String> = cols.iter().map(|c| esc(&r[*c])).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    Ok(out)
}

/// A consistent copy of the whole book, safe to take while the app is running. `VACUUM INTO` is the
/// supported way to snapshot a live SQLite database -- copying the file by hand can catch a WAL
/// mid-write and produce something that opens but is missing the last transactions.
pub fn snapshot_db(conn: &Connection, path: &str) -> Result<(), ExportError> {
    if std::path::Path::new(path).exists() {
        return Err(ExportError::Io(format!("{path} already exists")));
    }
    conn.execute("VACUUM INTO ?1", [path]).map_err(ExportError::Sql)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{self, NewPosting, NewTxn};

    fn book() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES
               (1,'Current','asset','GBP'), (2,'Salary','income','GBP'),
               (3,'Rent','expense','GBP'), (4,'Yen pot','asset','JPY');",
        )
        .unwrap();
        entry::create(&mut conn, &NewTxn {
            occurred_on: "2026-08-01".into(), description: "August salary".into(),
            payee: Some("Acme Ltd".into()), note: None,
            postings: vec![NewPosting { account_id: 1, amount_minor: 250_000 },
                           NewPosting { account_id: 2, amount_minor: -250_000 }],
            series_id: None,
            occurrence_on: None,
        }).unwrap();
        entry::create(&mut conn, &NewTxn {
            occurred_on: "2026-08-02".into(), description: "Rent".into(),
            payee: None, note: None,
            postings: vec![NewPosting { account_id: 1, amount_minor: -90_000 },
                           NewPosting { account_id: 3, amount_minor: 90_000 }],
            series_id: None,
            occurrence_on: None,
        }).unwrap();
        conn
    }

    fn opts<'a>() -> Options<'a> {
        Options { from: None, to: None, redact: false, forecast_to: None }
    }

    #[test]
    fn the_bundle_leads_with_the_traps_a_reader_would_fall_into() {
        let c = book();
        let b = bundle(&c, &opts()).unwrap();
        let pre = b["read_this_first"].as_array().unwrap();
        let all = pre.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>().join(" ");
        // Each of these is a mistake a model reliably makes on raw ledger data.
        assert!(all.contains("SUM TO ZERO"), "double-entry must be stated");
        assert!(all.contains("NOT always 100"), "the minor-unit trap must be stated");
        assert!(all.contains("never be summed together"), "cross-currency must be forbidden");
        assert!(all.contains("is_projection"), "forecast rows must be called out");
    }

    #[test]
    fn currencies_are_declared_with_their_scale() {
        let c = book();
        let b = bundle(&c, &opts()).unwrap();
        let curs = b["currencies"].as_array().unwrap();
        let jpy = curs.iter().find(|c| c["code"] == "JPY").unwrap();
        assert_eq!(jpy["minor_digits"], 0, "a reader dividing JPY by 100 would be 100x wrong");
        let gbp = curs.iter().find(|c| c["code"] == "GBP").unwrap();
        assert_eq!(gbp["minor_digits"], 2);
    }

    #[test]
    fn lines_carry_both_the_integer_and_a_ready_made_decimal() {
        let c = book();
        let b = bundle(&c, &opts()).unwrap();
        let lines = b["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 4, "two transactions, two postings each");
        let salary = lines.iter().find(|l| l["account"] == "Current").unwrap();
        assert_eq!(salary["amount_minor"], 250_000);
        assert_eq!(salary["amount_decimal"], "2500.00");
        assert_eq!(salary["is_projection"], false);
    }

    #[test]
    fn the_account_summary_lets_a_reader_check_its_own_arithmetic() {
        let c = book();
        let b = bundle(&c, &opts()).unwrap();
        let facts = b["account_summary"].as_array().unwrap();
        let current = facts.iter().find(|f| f["account"] == "Current").unwrap();
        assert_eq!(current["closing_minor"], 160_000, "2500 in, 900 out");
        assert_eq!(current["posting_count"], 2);
        assert_eq!(current["first_on"], "2026-08-01");
    }

    #[test]
    fn redaction_keeps_the_numbers_and_drops_the_story() {
        let c = book();
        let b = bundle(&c, &Options { redact: true, ..opts() }).unwrap();
        for l in b["lines"].as_array().unwrap() {
            assert_eq!(l["description"], "[redacted]");
            assert!(l["payee"].is_null());
            // the shape survives, which is the point -- it is still analysable
            assert!(l["amount_minor"].is_i64());
            assert!(l["account"].is_string());
        }
    }

    #[test]
    fn a_date_window_filters_the_lines() {
        let c = book();
        let b = bundle(&c, &Options { from: Some("2026-08-02"), ..opts() }).unwrap();
        assert_eq!(b["lines"].as_array().unwrap().len(), 2, "only the rent transaction");
    }

    #[test]
    fn forecast_rows_are_included_only_on_request_and_always_flagged() {
        let mut c = book();
        c.execute_batch(
            "INSERT INTO series(id,description,rrule,dtstart) VALUES(1,'Rent','FREQ=MONTHLY;BYMONTHDAY=1','2026-01-01');
             INSERT INTO series_posting(series_id,account_id,currency,amount_minor,role)
               VALUES(1,1,'GBP',-90000,'primary'),(1,3,'GBP',90000,'balancing');",
        ).unwrap();

        let without = bundle(&c, &opts()).unwrap();
        assert!(without["lines"].as_array().unwrap().iter().all(|l| l["is_projection"] == false));

        let with = bundle(&c, &Options {
            to: Some("2026-08-31"), forecast_to: Some("2026-11-30"), ..opts()
        }).unwrap();
        let projected: Vec<_> = with["lines"].as_array().unwrap().iter()
            .filter(|l| l["is_projection"] == true).collect();
        assert!(!projected.is_empty(), "asked for a forecast, got none");
        for p in projected {
            assert_eq!(p["source"], "forecast");
            assert!(p["txn_id"].is_null(), "a projection has no transaction id -- it has not happened");
        }
    }

    #[test]
    fn csv_escapes_what_it_must() {
        let mut c = book();
        entry::create(&mut c, &NewTxn {
            occurred_on: "2026-08-03".into(),
            description: "SMITH, JOHN & CO \"trading\"".into(),
            payee: None, note: None,
            postings: vec![NewPosting { account_id: 1, amount_minor: -100 },
                           NewPosting { account_id: 3, amount_minor: 100 }],
            series_id: None,
            occurrence_on: None,
        }).unwrap();
        let text = csv(&c, &opts()).unwrap();
        assert!(text.starts_with("on_date,txn_id,account,"));
        assert!(text.contains("\"SMITH, JOHN & CO \"\"trading\"\"\""), "got:\n{text}");
        assert_eq!(text.lines().count(), 7, "header + 6 postings");
    }

    #[test]
    fn a_snapshot_is_a_readable_book_and_refuses_to_clobber() {
        let c = book();
        let dir = std::env::temp_dir().join(format!("minka-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snap.db");
        let _ = std::fs::remove_file(&path);
        let p = path.to_str().unwrap();

        snapshot_db(&c, p).unwrap();
        let copy = Connection::open(p).unwrap();
        let n: i64 = copy.query_row("SELECT COUNT(*) FROM txn", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "the snapshot is a real, queryable book");

        // and it will not silently overwrite an existing file
        assert!(snapshot_db(&c, p).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
