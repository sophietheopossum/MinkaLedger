//! minka-ledger — stdio NDJSON server for the forecast-first ledger.
//!
//! Protocol is MinkaLink's, verbatim, so a QML frontend can reuse the same client shape:
//!   in   { "id": n, "method": "...", "params": {...} }   expects a response
//!        { "method": "...", "params": {...} }            fire-and-forget
//!   out  { "id": n, "result": ... } | { "id": n, "error": {...} }
//!        { "event": "...", "payload": ... }              broadcast
//!
//! Single-threaded, one connection held for process life. A panic in a handler is caught and
//! returned as an error: a malformed request must not take the session down.

mod analysis;
mod db;
mod recur;
use crate::recur::Recurrence; // series.preview calls expand() directly
mod entry;
mod export;
mod forecast;
mod fx;
mod importer;
mod interest;
mod journey;
mod link;
mod money;

use std::io::{BufRead, Write};

fn main() {
    // The book path: --db PATH, else $MINKA_LEDGER_DB, else a file beside the user's data dir.
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .iter()
        .position(|a| a == "--db")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("MINKA_LEDGER_DB").ok())
        .unwrap_or_else(default_db_path);

    let mut conn = match db::open(&path) {
        Ok(c) => c,
        Err(e) => {
            // Fatal and worth saying loudly: every method below needs the book.
            eprintln!("minka-ledger: cannot open {path}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("minka-ledger: book at {path} (schema v{})", db::current_version());

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&mut conn, line) {
            let _ = writeln!(out, "{reply}");
            let _ = out.flush();
        }
    }
}

fn default_db_path() -> String {
    let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        format!("{}/.local/share", std::env::var("HOME").unwrap_or_else(|_| ".".into()))
    });
    let dir = format!("{base}/minka-ledger");
    let _ = std::fs::create_dir_all(&dir);
    format!("{dir}/book.db")
}

fn handle_line(conn: &mut rusqlite::Connection, line: &str) -> Option<String> {
    let msg: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None, // malformed lines are ignored, as ShojiClient does
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);

    // Catch a panic so one bad request cannot end the session.
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(conn, method, params)));
    let result = match outcome {
        Ok(r) => r,
        Err(_) => Err(Error::internal("handler panicked")),
    };

    let id = id?; // no id => fire-and-forget, nothing to reply to
    Some(match result {
        Ok(value) => serde_json::json!({ "id": id, "result": value }).to_string(),
        Err(e) => serde_json::json!({ "id": id, "error": { "code": e.code, "message": e.message } })
            .to_string(),
    })
}

struct Error {
    code: &'static str,
    message: String,
}
impl Error {
    fn internal(m: &str) -> Self {
        Error { code: "internal", message: m.to_string() }
    }
    fn unknown_method(m: &str) -> Self {
        Error { code: "unknown_method", message: format!("no such method: {m}") }
    }
}

impl From<entry::EntryError> for Error {
    fn from(e: entry::EntryError) -> Self {
        use entry::EntryError as E;
        let code = match e {
            E::Unbalanced(_) => "unbalanced",
            E::TooFewPostings => "too_few_postings",
            E::NoSuchAccount(_) => "no_such_account",
            E::SystemAccount(_) => "system_account",
            E::NotFound(_) => "not_found",
            E::Sql(_) => "sql",
        };
        Error { code, message: e.to_string() }
    }
}

impl From<journey::JourneyError> for Error {
    fn from(e: journey::JourneyError) -> Self {
        use journey::JourneyError as J;
        let code = match e {
            J::NotFound(_) => "not_found",
            J::NoSuchTxn(_) => "no_such_txn",
            J::Sql(_) => "sql",
        };
        Error { code, message: e.to_string() }
    }
}

impl From<fx::FxError> for Error {
    fn from(e: fx::FxError) -> Self {
        use fx::FxError as F;
        let code = match e {
            F::NoRate { .. } => "no_rate",
            F::UnknownCurrency(_) => "unknown_currency",
            F::SameCurrency(_) => "same_currency",
            F::BadRate(_) => "bad_rate",
            F::Sql(_) => "sql",
        };
        Error { code, message: e.to_string() }
    }
}

impl From<importer::ImportError> for Error {
    fn from(e: importer::ImportError) -> Self {
        use importer::ImportError as I;
        let code = match e {
            I::NoProfile(_) => "no_profile",
            I::NoBatch(_) => "no_batch",
            I::BadMapping(_) => "bad_mapping",
            I::AlreadyCommitted(_) => "already_committed",
            I::Csv(_) => "csv",
            I::Sql(_) => "sql",
        };
        Error { code, message: e.to_string() }
    }
}

impl From<export::ExportError> for Error {
    fn from(e: export::ExportError) -> Self {
        let code = match e { export::ExportError::Io(_) => "io", _ => "sql" };
        Error { code, message: e.to_string() }
    }
}

impl From<analysis::AnalysisError> for Error {
    fn from(e: analysis::AnalysisError) -> Self {
        let code = match e {
            analysis::AnalysisError::Bad(_) => "bad_query",
            analysis::AnalysisError::Sql(_) => "sql",
        };
        Error { code, message: e.to_string() }
    }
}

impl From<link::LinkError> for Error {
    fn from(e: link::LinkError) -> Self {
        use link::LinkError as L;
        let code = match e {
            L::NoSuchTxn(_) => "no_such_txn",
            L::SelfLink => "self_link",
            L::Exists(_, _) => "already_linked",
            L::Sql(_) | L::Unexpected(_) => "sql",
        };
        Error { code, message: e.to_string() }
    }
}

impl From<db::DbError> for Error {
    fn from(e: db::DbError) -> Self {
        Error { code: "db", message: e.to_string() }
    }
}

fn bad(msg: &str) -> Error {
    Error { code: "bad_params", message: msg.to_string() }
}

fn dispatch(
    conn: &mut rusqlite::Connection,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    match method {
        "health.ping" => Ok(serde_json::json!({
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "max_minor_digits": money::MAX_MINOR_DIGITS,
        })),

        // The frontend validates amounts the operator types by asking the core, so parsing lives in
        // exactly one place. A QML reimplementation would be a second rounding rule waiting to
        // disagree with this one at the half-penny.
        "money.parse" => {
            let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let digits = params.get("minor_digits").and_then(|v| v.as_u64()).unwrap_or(2) as u32;
            match money::parse_minor(text, digits) {
                Ok(minor) => Ok(serde_json::json!({
                    "minor": minor,
                    "formatted": money::format_minor(minor, digits),
                })),
                Err(e) => Err(Error { code: "bad_amount", message: e.to_string() }),
            }
        }

        "money.format" => {
            let minor = params.get("minor").and_then(|v| v.as_i64()).unwrap_or(0);
            let digits = params.get("minor_digits").and_then(|v| v.as_u64()).unwrap_or(2) as u32;
            if digits > money::MAX_MINOR_DIGITS {
                return Err(Error { code: "bad_amount", message: format!("minor_digits {digits}") });
            }
            Ok(serde_json::json!({ "formatted": money::format_minor(minor, digits) }))
        }

        "db.check" => Ok(db::integrity(conn)?),

        // ---- accounts ----
        // ---- currencies ----
        //
        // minor_digits is the field that matters and the one nobody thinks about: it is the
        // divisor for every amount in that currency, so a wrong value is a silent 100x error
        // rather than a visible failure. It is therefore never inferred from anything -- the
        // caller states it, and the frontend offers the ISO value rather than a free-text box.
        "currency.list" => {
            let mut stmt = conn
                .prepare(
                    // is_display travels with the row so the UI can omit a remove control that
                    // could only ever be refused, rather than offering one and explaining after.
                    "SELECT c.code, c.minor_digits, c.name,
                            (SELECT COUNT(*) FROM account WHERE currency = c.code),
                            (SELECT COUNT(*) FROM posting WHERE currency = c.code),
                            c.code = (SELECT value FROM book_meta WHERE key = 'display_currency')
                       FROM currency c ORDER BY c.code",
                )
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map([], |r| {
                    Ok(serde_json::json!({
                        "code": r.get::<_, String>(0)?,
                        "minor_digits": r.get::<_, i64>(1)?,
                        "name": r.get::<_, String>(2)?,
                        "accounts": r.get::<_, i64>(3)?,
                        "postings": r.get::<_, i64>(4)?,
                        "is_display": r.get::<_, i64>(5)? == 1,
                    }))
                })
                .and_then(|m| m.collect())
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::Value::Array(rows))
        }

        "currency.create" => {
            let code = params
                .get("code")
                .and_then(|v| v.as_str())
                .ok_or_else(|| bad("code"))?
                .trim()
                .to_uppercase();
            // The schema requires exactly three characters; checking here as well turns a raw
            // CHECK failure into something the operator can act on.
            if code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(bad("code must be three letters, e.g. CHF"));
            }
            let digits = params
                .get("minor_digits")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| bad("minor_digits"))?;
            if !(0..=money::MAX_MINOR_DIGITS as i64).contains(&digits) {
                return Err(bad(&format!(
                    "minor_digits must be 0..={} -- an 18dp currency overflows i64 at 9.22 units",
                    money::MAX_MINOR_DIGITS
                )));
            }
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or(&code);
            conn.execute(
                "INSERT INTO currency(code, minor_digits, name) VALUES(?1,?2,?3)",
                rusqlite::params![code, digits, name],
            )
            .map_err(|e| Error {
                code: if e.to_string().contains("UNIQUE") { "already_exists" } else { "sql" },
                message: if e.to_string().contains("UNIQUE") {
                    format!("{code} already exists")
                } else {
                    e.to_string()
                },
            })?;
            Ok(serde_json::json!({ "code": code, "minor_digits": digits, "name": name }))
        }

        // A currency in use cannot go: every posting's amount is meaningless without its scale.
        "currency.delete" => {
            let code = params.get("code").and_then(|v| v.as_str()).ok_or_else(|| bad("code"))?;
            let count = |sql: &str| -> Result<i64, Error> {
                conn.query_row(sql, [code], |r| r.get(0))
                    .map_err(|e| Error { code: "sql", message: e.to_string() })
            };
            let uses = count("SELECT COUNT(*) FROM account WHERE currency = ?1")?
                + count("SELECT COUNT(*) FROM posting WHERE currency = ?1")?
                + count("SELECT COUNT(*) FROM series_posting WHERE currency = ?1")?
                + count("SELECT COUNT(*) FROM fx_rate WHERE base_code = ?1 OR quote_code = ?1")?
                + count("SELECT COUNT(*) FROM import_profile WHERE default_currency = ?1")?;
            if uses > 0 {
                return Err(Error {
                    code: "in_use",
                    message: format!("{code} is used by {uses} record(s) and cannot be removed"),
                });
            }
            let display: String = conn
                .query_row("SELECT value FROM book_meta WHERE key = 'display_currency'", [], |r| {
                    r.get(0)
                })
                .unwrap_or_default();
            if display == code {
                return Err(Error {
                    code: "in_use",
                    message: format!("{code} is the book's display currency"),
                });
            }
            let n = conn
                .execute("DELETE FROM currency WHERE code = ?1", [code])
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            if n == 0 {
                return Err(Error { code: "not_found", message: format!("no such currency: {code}") });
            }
            Ok(serde_json::json!({ "deleted": code }))
        }

        "account.create" => {
            let name = params.get("name").and_then(|v| v.as_str()).ok_or_else(|| bad("name"))?;
            let kind = params.get("kind").and_then(|v| v.as_str()).ok_or_else(|| bad("kind"))?;
            let cur = params.get("currency").and_then(|v| v.as_str()).unwrap_or("GBP");
            let parent = params.get("parent_id").and_then(|v| v.as_i64());
            conn.execute(
                "INSERT INTO account(name, kind, currency, parent_id) VALUES(?1,?2,?3,?4)",
                rusqlite::params![name, kind, cur, parent],
            )
            .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": conn.last_insert_rowid() }))
        }

        "account.list" => {
            let mut stmt = conn
                .prepare(
                    // The counts decide close-vs-delete, and they are what the operator is
                    // shown when delete is refused. Gathered here so the UI never has to guess
                    // and never has to ask twice.
                    "SELECT a.id, a.name, a.kind, a.currency, a.parent_id, a.system, a.closed,
                            (SELECT COUNT(*) FROM posting        WHERE account_id = a.id),
                            (SELECT COUNT(*) FROM series_posting WHERE account_id = a.id),
                            (SELECT COUNT(*) FROM interest_rule
                              WHERE account_id = a.id OR counter_account_id = a.id)
                          + (SELECT COUNT(*) FROM payment_rule
                              WHERE account_id = a.id OR from_account_id = a.id),
                            (SELECT COUNT(*) FROM import_profile WHERE account_id = a.id)
                          + (SELECT COUNT(*) FROM import_row
                              WHERE account_id = a.id OR far_account_id = a.id)
                          + (SELECT COUNT(*) FROM import_rule WHERE set_far_account_id = a.id)
                          + (SELECT COUNT(*) FROM txn_import_key WHERE account_id = a.id),
                            (SELECT COUNT(*) FROM account WHERE parent_id = a.id)
                       FROM account a ORDER BY a.kind, a.name",
                )
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map([], |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, i64>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "kind": r.get::<_, String>(2)?,
                        "currency": r.get::<_, String>(3)?,
                        "parent_id": r.get::<_, Option<i64>>(4)?,
                        "system": r.get::<_, i64>(5)? == 1,
                        "closed": r.get::<_, i64>(6)? == 1,
                        "postings": r.get::<_, i64>(7)?,
                        "series": r.get::<_, i64>(8)?,
                        "rules": r.get::<_, i64>(9)?,
                        "imports": r.get::<_, i64>(10)?,
                        "children": r.get::<_, i64>(11)?,
                    }))
                })
                .and_then(|m| m.collect())
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::Value::Array(rows))
        }

        "account.close" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            let closed = params.get("closed").and_then(|v| v.as_bool()).unwrap_or(true);
            conn.execute("UPDATE account SET closed = ?2 WHERE id = ?1", rusqlite::params![id, closed as i64])
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": id, "closed": closed }))
        }

        // Removing an account has TWO answers and the difference is not cosmetic.
        //
        // An account with history must be CLOSED, never deleted: its postings are half of every
        // transaction it took part in, and destroying them would unbalance the book permanently --
        // the one thing this ledger exists to make impossible. account.close already does that, and
        // is reversible.
        //
        // An account with no references at all is a different thing: a typo, or a category created
        // and never used. Deleting that loses nothing, so it is allowed.
        //
        // The counts are gathered before the attempt so the refusal can say WHAT is in the way
        // rather than "FOREIGN KEY constraint failed". The DELETE still runs inside a transaction
        // with foreign_keys ON, so anything this list has forgotten is caught by the database
        // rather than silently corrupting the book.
        "account.delete" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            let (name, system): (String, i64) = conn
                .query_row("SELECT name, system FROM account WHERE id = ?1", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(|_| Error {
                    code: "not_found",
                    message: format!("no such account: {id}"),
                })?;
            if system == 1 {
                return Err(Error {
                    code: "system_account",
                    message: format!("{name} is a system account and cannot be removed"),
                });
            }

            let count = |sql: &str| -> Result<i64, Error> {
                conn.query_row(sql, [id], |r| r.get(0))
                    .map_err(|e| Error { code: "sql", message: e.to_string() })
            };
            let uses = [
                ("transaction", count("SELECT COUNT(*) FROM posting WHERE account_id = ?1")?),
                ("recurring payment",
                 count("SELECT COUNT(*) FROM series_posting WHERE account_id = ?1")?),
                ("interest or payment rule",
                 count("SELECT COUNT(*) FROM interest_rule WHERE account_id = ?1 OR counter_account_id = ?1")?
                 + count("SELECT COUNT(*) FROM payment_rule WHERE account_id = ?1 OR from_account_id = ?1")?),
                ("import record",
                 count("SELECT COUNT(*) FROM import_profile WHERE account_id = ?1")?
                 + count("SELECT COUNT(*) FROM import_row WHERE account_id = ?1 OR far_account_id = ?1")?
                 + count("SELECT COUNT(*) FROM import_rule WHERE set_far_account_id = ?1")?
                 + count("SELECT COUNT(*) FROM txn_import_key WHERE account_id = ?1")?),
                ("child account", count("SELECT COUNT(*) FROM account WHERE parent_id = ?1")?),
            ];
            let blocking: Vec<String> = uses
                .iter()
                .filter(|(_, n)| *n > 0)
                .map(|(what, n)| format!("{n} {what}{}", if *n == 1 { "" } else { "s" }))
                .collect();
            if !blocking.is_empty() {
                return Err(Error {
                    code: "in_use",
                    message: format!(
                        "{name} has {} — close it instead, which hides it and keeps the history",
                        blocking.join(" and ")
                    ),
                });
            }

            conn.execute("DELETE FROM account WHERE id = ?1", [id])
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "deleted": id, "name": name }))
        }

        "account.balances" => {
            let as_of = params.get("as_of").and_then(|v| v.as_str());
            Ok(serde_json::Value::Array(entry::balances(conn, as_of)?))
        }

        // ---- transactions ----
        "txn.create" => {
            let new: entry::NewTxn = serde_json::from_value(params)
                .map_err(|e| bad(&format!("{e}")))?;
            let id = entry::create(conn, &new)?;
            Ok(serde_json::json!({ "id": id }))
        }

        "txn.get" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            Ok(entry::get(conn, id)?)
        }

        "txn.list" => {
            let from = params.get("from").and_then(|v| v.as_str());
            let to = params.get("to").and_then(|v| v.as_str());
            let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(200);
            Ok(serde_json::Value::Array(entry::list(conn, from, to, limit)?))
        }

        "txn.delete" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            entry::delete(conn, id)?;
            Ok(serde_json::json!({ "deleted": id }))
        }

        // Expand a rule WITHOUT saving anything, so a form can show what it will actually do.
        // Typing an RRULE blind and finding out next month is a poor way to learn RFC 5545.
        "series.preview" => {
            let rrule = params.get("rrule").and_then(|v| v.as_str()).ok_or_else(|| bad("rrule"))?;
            let dtstart = params.get("dtstart").and_then(|v| v.as_str()).ok_or_else(|| bad("dtstart"))?;
            let start = chrono::NaiveDate::parse_from_str(dtstart, "%Y-%m-%d")
                .map_err(|_| bad("dtstart must be YYYY-MM-DD"))?;
            let count = params.get("count").and_then(|v| v.as_i64()).unwrap_or(6).clamp(1, 50);
            let until = params.get("until_on").and_then(|v| v.as_str())
                .and_then(|u| chrono::NaiveDate::parse_from_str(u, "%Y-%m-%d").ok());
            let weekend = params.get("weekend_rule").and_then(|v| v.as_str()).unwrap_or("none");
            // A generous horizon so even an annual rule yields `count` dates.
            let horizon = start + chrono::Duration::days(366 * 12);
            let holidays: Vec<chrono::NaiveDate> = Vec::new();
            let dates = recur::RRuleCrate
                .expand(rrule, start, until, start, horizon)
                .map_err(|e| Error { code: "bad_rule", message: e.to_string() })?;
            let out: Vec<serde_json::Value> = dates
                .into_iter()
                .take(count as usize)
                .map(|d| {
                    let adjusted = recur::business_adjust(d, weekend, &holidays);
                    serde_json::json!({
                        "occurrence_on": d.to_string(),
                        "value_on": adjusted.to_string(),
                        "moved": adjusted != d,
                    })
                })
                .collect();
            Ok(serde_json::json!({ "dates": out }))
        }

        // ---- recurring series (req 1) ----
        "series.create" => {
            let desc = params.get("description").and_then(|v| v.as_str()).ok_or_else(|| bad("description"))?;
            let rrule = params.get("rrule").and_then(|v| v.as_str()).ok_or_else(|| bad("rrule"))?;
            let dtstart = params.get("dtstart").and_then(|v| v.as_str()).ok_or_else(|| bad("dtstart"))?;
            let until = params.get("until_on").and_then(|v| v.as_str());
            let weekend = params.get("weekend_rule").and_then(|v| v.as_str()).unwrap_or("none");
            let scenario = params.get("scenario_id").and_then(|v| v.as_i64());
            let supersedes = params.get("supersedes_id").and_then(|v| v.as_i64());
            let postings = params.get("postings").and_then(|v| v.as_array()).ok_or_else(|| bad("postings"))?.clone();

            let tx = conn.transaction().map_err(|e| Error { code: "sql", message: e.to_string() })?;
            tx.execute(
                "INSERT INTO series(description, rrule, dtstart, until_on, weekend_rule, scenario_id, supersedes_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![desc, rrule, dtstart, until, weekend, scenario, supersedes],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let sid = tx.last_insert_rowid();
            for p in &postings {
                let acc = p.get("account_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("account_id"))?;
                let amt = p.get("amount_minor").and_then(|v| v.as_i64()).ok_or_else(|| bad("amount_minor"))?;
                let role = p.get("role").and_then(|v| v.as_str()).unwrap_or("other");
                // Currency comes from the account, never from the caller -- same rule as entry.rs.
                let cur: String = tx.query_row("SELECT currency FROM account WHERE id = ?1", [acc], |r| r.get(0))
                    .map_err(|_| Error { code: "no_such_account", message: format!("no such account: {acc}") })?;
                tx.execute(
                    "INSERT INTO series_posting(series_id, account_id, currency, amount_minor, role)
                     VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![sid, acc, cur, amt, role],
                ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            }
            tx.commit().map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": sid }))
        }

        "series.list" => {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.description, s.rrule, s.dtstart, s.until_on, s.weekend_rule, s.scenario_id,
                        (SELECT COUNT(*) FROM series_posting sp WHERE sp.series_id = s.id),
                        s.supersedes_id,
                        -- The name of what it supersedes, not just the id: a scenario row reads as
                        -- \"cancels Netflix\" or it reads as \"supersedes_id: 7\", and only one of
                        -- those is something an operator can check.
                        (SELECT o.description FROM series o WHERE o.id = s.supersedes_id),
                        -- The primary leg is the money the series moves; the balancing leg is the
                        -- same figure seen from the other side.
                        (SELECT sp.amount_minor FROM series_posting sp
                          WHERE sp.series_id = s.id AND sp.role = 'primary'),
                        -- The currency travels with the amount: a series is denominated in its
                        -- primary account's currency and a caller must not assume 2dp GBP.
                        (SELECT sp.currency FROM series_posting sp
                          WHERE sp.series_id = s.id AND sp.role = 'primary')
                   FROM series s ORDER BY s.id",
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rows: Vec<serde_json::Value> = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "description": r.get::<_, String>(1)?,
                    "rrule": r.get::<_, String>(2)?,
                    "dtstart": r.get::<_, String>(3)?,
                    "until_on": r.get::<_, Option<String>>(4)?,
                    "weekend_rule": r.get::<_, String>(5)?,
                    "scenario_id": r.get::<_, Option<i64>>(6)?,
                    "postings": r.get::<_, i64>(7)?,
                    "supersedes_id": r.get::<_, Option<i64>>(8)?,
                    "supersedes": r.get::<_, Option<String>>(9)?,
                    "amount_minor": r.get::<_, Option<i64>>(10)?,
                    "currency": r.get::<_, Option<String>>(11)?,
                }))
            }).and_then(|m| m.collect()).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::Value::Array(rows))
        }

        // Bound a series that is already running, or unbound it again.
        //
        // until_on is INCLUSIVE (RFC 5545 3.3.10): an occurrence falling exactly on it still
        // happens. There is deliberately no "after N payments" here -- RFC COUNT counts generated
        // occurrences BEFORE EXDATE removal, so skipping one instalment of "12 payments" silently
        // yields 11. A frontend that wants to ask the question that way should expand the rule,
        // take the Nth date, and send that.
        "series.end" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            let until = params.get("until_on").and_then(|v| v.as_str());
            let dtstart: String = conn
                .query_row("SELECT dtstart FROM series WHERE id = ?1", [id], |r| r.get(0))
                .map_err(|_| Error { code: "not_found", message: format!("no such series: {id}") })?;
            if let Some(u) = until {
                if chrono::NaiveDate::parse_from_str(u, "%Y-%m-%d").is_err() {
                    return Err(bad("until_on must be YYYY-MM-DD"));
                }
                // The schema CHECK would catch this, but "ends before it starts" is worth saying
                // in those words rather than as a constraint name.
                if u < dtstart.as_str() {
                    return Err(Error {
                        code: "bad_params",
                        message: format!("it would end on {u}, before it starts on {dtstart}"),
                    });
                }
            }
            conn.execute(
                "UPDATE series SET until_on = ?2 WHERE id = ?1",
                rusqlite::params![id, until],
            )
            .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": id, "until_on": until }))
        }

        // req 4: alter or skip ONE occurrence
        "series.override" => {
            let sid = params.get("series_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("series_id"))?;
            let on = params.get("occurrence_on").and_then(|v| v.as_str()).ok_or_else(|| bad("occurrence_on"))?;
            let action = params.get("action").and_then(|v| v.as_str()).unwrap_or("amend");
            let moved = params.get("moved_to").and_then(|v| v.as_str());
            let amount = params.get("amount_minor").and_then(|v| v.as_i64());
            let desc = params.get("description").and_then(|v| v.as_str());
            conn.execute(
                "INSERT INTO series_override(series_id, occurrence_on, action, moved_to, amount_minor, description)
                 VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(series_id, occurrence_on) DO UPDATE SET
                   action=excluded.action, moved_to=excluded.moved_to,
                   amount_minor=excluded.amount_minor, description=excluded.description",
                rusqlite::params![sid, on, action, moved, amount, desc],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "series_id": sid, "occurrence_on": on, "action": action }))
        }

        "series.clear_override" => {
            let sid = params.get("series_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("series_id"))?;
            let on = params.get("occurrence_on").and_then(|v| v.as_str()).ok_or_else(|| bad("occurrence_on"))?;
            let n = conn.execute(
                "DELETE FROM series_override WHERE series_id = ?1 AND occurrence_on = ?2",
                rusqlite::params![sid, on],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "cleared": n }))
        }

        // ---- scenarios (req 8) ----
        "scenario.create" => {
            let name = params.get("name").and_then(|v| v.as_str()).ok_or_else(|| bad("name"))?;
            let note = params.get("note").and_then(|v| v.as_str());
            conn.execute("INSERT INTO scenario(name, note) VALUES(?1,?2)", rusqlite::params![name, note])
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": conn.last_insert_rowid() }))
        }

        // Deleting a scenario takes its series with it, by ON DELETE CASCADE on
        // series.scenario_id -- which is what makes a scenario safe to try. Baseline series are
        // untouched by construction: a scenario can only ever ADD rows or SUPERSEDE a baseline
        // one, and superseding is a property of the scenario row, so removing it restores the
        // baseline with nothing to undo.
        "scenario.delete" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            let n = conn
                .execute("DELETE FROM scenario WHERE id = ?1", [id])
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            if n == 0 {
                return Err(Error { code: "not_found", message: format!("no such scenario: {id}") });
            }
            Ok(serde_json::json!({ "deleted": id }))
        }

        "scenario.list" => {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.name, s.note,
                        (SELECT COUNT(*) FROM series WHERE scenario_id = s.id),
                        (SELECT COUNT(*) FROM series WHERE scenario_id = s.id
                                                       AND supersedes_id IS NOT NULL)
                   FROM scenario s ORDER BY s.id")
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rows: Vec<serde_json::Value> = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?,
                    "note": r.get::<_, Option<String>>(2)?,
                    // A scenario with no series changes nothing when activated. Saying so is the
                    // difference between "off" and "on but empty", which look identical otherwise.
                    "series_count": r.get::<_, i64>(3)?,
                    "supersedes_count": r.get::<_, i64>(4)?,
                }))
            }).and_then(|m| m.collect()).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::Value::Array(rows))
        }

        // ---- the projection (req 2, 8) ----
        "forecast.project" => {
            let parse = |k: &str| -> Result<chrono::NaiveDate, Error> {
                let s = params.get(k).and_then(|v| v.as_str()).ok_or_else(|| bad(k))?;
                chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .map_err(|_| bad(&format!("{k} must be YYYY-MM-DD")))
            };
            let as_of = parse("as_of")?;
            let horizon = parse("horizon")?;
            let active: std::collections::HashSet<i64> = params
                .get("scenarios").and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            let snap = forecast::load::snapshot(conn, as_of)
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let proj = forecast::project(&recur::RRuleCrate, &snap, as_of, horizon, &active)
                .map_err(|e| Error { code: "bad_rule", message: e.to_string() })?;
            serde_json::to_value(proj).map_err(|e| Error { code: "internal", message: e.to_string() })
        }

        // ---- journeys (req 5, 10) ----
        "journey.create" => {
            let label = params.get("label").and_then(|v| v.as_str()).ok_or_else(|| bad("label"))?;
            let on = params.get("opened_on").and_then(|v| v.as_str()).ok_or_else(|| bad("opened_on"))?;
            Ok(serde_json::json!({ "id": journey::create(conn, label, on)? }))
        }
        "journey.list" => Ok(serde_json::Value::Array(journey::list(conn)?)),
        "journey.get" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            Ok(journey::get(conn, id)?)
        }
        "journey.attach" => {
            let j = params.get("journey_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("journey_id"))?;
            let t = params.get("txn_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("txn_id"))?;
            let seq = params.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
            let role = params.get("role").and_then(|v| v.as_str()).unwrap_or("leg");
            journey::attach(conn, j, t, seq, role)?;
            Ok(serde_json::json!({ "journey_id": j, "txn_id": t, "seq": seq, "role": role }))
        }
        "journey.detach" => {
            let j = params.get("journey_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("journey_id"))?;
            let t = params.get("txn_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("txn_id"))?;
            Ok(serde_json::json!({ "detached": journey::detach(conn, j, t)? }))
        }
        "journey.close" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            let on = params.get("on").and_then(|v| v.as_str()).ok_or_else(|| bad("on"))?;
            journey::close(conn, id, on)?;
            Ok(serde_json::json!({ "id": id, "closed_on": on }))
        }
        "journey.for_txn" => {
            let t = params.get("txn_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("txn_id"))?;
            Ok(serde_json::Value::Array(journey::for_txn(conn, t)?))
        }

        // ---- multi-currency ----
        "fx.put_rate" => {
            let base = params.get("base").and_then(|v| v.as_str()).ok_or_else(|| bad("base"))?;
            let quote = params.get("quote").and_then(|v| v.as_str()).ok_or_else(|| bad("quote"))?;
            let as_of = params.get("as_of").and_then(|v| v.as_str()).ok_or_else(|| bad("as_of"))?;
            let source = params.get("source").and_then(|v| v.as_str()).unwrap_or("manual");
            // Accept either an exact rational or a decimal string, which is parsed EXACTLY into one.
            let rate = if let (Some(n), Some(d)) =
                (params.get("num").and_then(|v| v.as_i64()), params.get("den").and_then(|v| v.as_i64()))
            {
                fx::Rate { num: n, den: d }
            } else {
                let text = params.get("rate").and_then(|v| v.as_str()).ok_or_else(|| bad("rate or num/den"))?;
                let (whole, frac) = match text.split_once('.') {
                    Some((w, f)) => (w, f),
                    None => (text, ""),
                };
                let digits: String = format!("{whole}{frac}");
                let num: i64 = digits.parse().map_err(|_| bad("rate must be a decimal"))?;
                fx::Rate { num, den: 10i64.pow(frac.len() as u32) }
            };
            fx::put_rate(conn, base, quote, as_of, source, rate)?;
            Ok(serde_json::json!({ "base": base, "quote": quote, "as_of": as_of,
                                   "num": rate.num, "den": rate.den }))
        }

        "fx.rate" => {
            let base = params.get("base").and_then(|v| v.as_str()).ok_or_else(|| bad("base"))?;
            let quote = params.get("quote").and_then(|v| v.as_str()).ok_or_else(|| bad("quote"))?;
            let on = params.get("on").and_then(|v| v.as_str()).ok_or_else(|| bad("on"))?;
            let on = chrono::NaiveDate::parse_from_str(on, "%Y-%m-%d").map_err(|_| bad("on"))?;
            let (r, as_of) = fx::resolve_rate(conn, base, quote, on)?;
            Ok(serde_json::json!({ "num": r.num, "den": r.den, "as_of": as_of,
                                   "approx": r.as_f64_for_display() }))
        }

        "fx.convert" => {
            let amount = params.get("amount_minor").and_then(|v| v.as_i64()).ok_or_else(|| bad("amount_minor"))?;
            let from = params.get("from").and_then(|v| v.as_str()).ok_or_else(|| bad("from"))?;
            let to = params.get("to").and_then(|v| v.as_str()).ok_or_else(|| bad("to"))?;
            let on = params.get("on").and_then(|v| v.as_str()).ok_or_else(|| bad("on"))?;
            let on = chrono::NaiveDate::parse_from_str(on, "%Y-%m-%d").map_err(|_| bad("on"))?;
            let (out, r, as_of) = fx::convert(conn, amount, from, to, on)?;
            Ok(serde_json::json!({ "amount_minor": out, "rate_as_of": as_of,
                                   "num": r.num, "den": r.den }))
        }

        "fx.fetch" => {
            let on = params.get("on").and_then(|v| v.as_str())
                .map(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| bad("on")))
                .transpose()?
                .unwrap_or_else(|| chrono::Local::now().date_naive());
            let (reported, n) = fx::fetch::rates_for(conn, on)?;
            Ok(serde_json::json!({ "requested": on.to_string(), "reported": reported, "stored": n }))
        }

        "fx.position" => {
            let on = params.get("on").and_then(|v| v.as_str()).ok_or_else(|| bad("on"))?;
            let on = chrono::NaiveDate::parse_from_str(on, "%Y-%m-%d").map_err(|_| bad("on"))?;
            Ok(fx::fx_position(conn, on)?)
        }

        // The ONLY way to create a cross-currency transaction.
        "txn.convert" => {
            let on = params.get("occurred_on").and_then(|v| v.as_str()).ok_or_else(|| bad("occurred_on"))?;
            let desc = params.get("description").and_then(|v| v.as_str()).unwrap_or("currency conversion");
            let fa = params.get("from_account").and_then(|v| v.as_i64()).ok_or_else(|| bad("from_account"))?;
            let fm = params.get("from_minor").and_then(|v| v.as_i64()).ok_or_else(|| bad("from_minor"))?;
            let ta = params.get("to_account").and_then(|v| v.as_i64()).ok_or_else(|| bad("to_account"))?;
            let tm = params.get("to_minor").and_then(|v| v.as_i64()).ok_or_else(|| bad("to_minor"))?;
            let id = fx::build_conversion(conn, on, desc, fa, fm, ta, tm)?;
            Ok(serde_json::json!({ "id": id }))
        }

        // ---- interest (req 9) ----
        "interest.create_rule" => {
            let g = |k: &str| params.get(k).and_then(|v| v.as_i64());
            let gs = |k: &str| params.get(k).and_then(|v| v.as_str());
            let account = g("account_id").ok_or_else(|| bad("account_id"))?;
            let counter = g("counter_account_id").ok_or_else(|| bad("counter_account_id"))?;
            let shape = gs("shape").ok_or_else(|| bad("shape"))?;
            let accrues = gs("accrues_on").unwrap_or(if shape == "savings" { "positive" } else { "negative" });
            let freq = gs("accrual_freq").unwrap_or("daily");
            let cap_rrule = gs("capitalise_rrule").unwrap_or("FREQ=MONTHLY;BYMONTHDAY=1");
            let cap_start = gs("capitalise_dtstart").ok_or_else(|| bad("capitalise_dtstart"))?;
            let grace = params.get("grace_period").and_then(|v| v.as_bool()).unwrap_or(false);
            let scenario = g("scenario_id");

            // The rate: quoted annually, converted ONCE here and stored as an integer.
            let quoted = gs("quoted_rate").ok_or_else(|| bad("quoted_rate, e.g. \"24.9\" for 24.9%"))?;
            let basis = gs("rate_basis").unwrap_or("effective");
            let ppy = g("periods_per_year").unwrap_or(if freq == "daily" { 365 } else { 12 });
            let pct: f64 = quoted.parse().map_err(|_| bad("quoted_rate must be a number"))?;
            let quoted_e15 = (pct / 100.0 * interest::RATE_SCALE as f64).round() as i64;
            let periodic = interest::derive_periodic_rate(quoted_e15, basis, ppy)
                .map_err(|e| Error { code: "bad_rate", message: e.to_string() })?;

            let tx = conn.transaction().map_err(|e| Error { code: "sql", message: e.to_string() })?;
            tx.execute(
                "INSERT INTO interest_rule(account_id, counter_account_id, shape, accrues_on,
                    accrual_freq, capitalise_rrule, capitalise_dtstart, grace_period, scenario_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                rusqlite::params![account, counter, shape, accrues, freq, cap_rrule, cap_start,
                                  grace as i64, scenario],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rid = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO interest_rate_period(rule_id, effective_from, quoted_rate_e15,
                    rate_basis, periods_per_year, periodic_rate_e15)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![rid, cap_start, quoted_e15, basis, ppy, periodic],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            tx.commit().map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": rid, "periodic_rate_e15": periodic,
                                   "periods_per_year": ppy, "basis": basis }))
        }

        "payment.create_rule" => {
            let g = |k: &str| params.get(k).and_then(|v| v.as_i64());
            let gs = |k: &str| params.get(k).and_then(|v| v.as_str());
            let account = g("account_id").ok_or_else(|| bad("account_id"))?;
            let from = g("from_account_id").ok_or_else(|| bad("from_account_id"))?;
            let kind = gs("amount_kind").ok_or_else(|| bad("amount_kind"))?;
            let rrule = gs("rrule").ok_or_else(|| bad("rrule"))?;
            let dtstart = gs("dtstart").ok_or_else(|| bad("dtstart"))?;
            // A percentage arrives as "1.5" meaning 1.5%, and is stored scaled by 1e15.
            let pct = params.get("pct").and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| (p / 100.0 * interest::RATE_SCALE as f64).round() as i64);
            conn.execute(
                "INSERT INTO payment_rule(account_id, from_account_id, amount_kind, fixed_minor,
                    pct_e15, floor_minor, cap_minor, level_payment_minor, rrule, dtstart, until_on,
                    interest_rule_id, due_offset_days, term_periods, scenario_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                rusqlite::params![account, from, kind, g("fixed_minor"), pct, g("floor_minor"),
                    g("cap_minor"), g("level_payment_minor"), rrule, dtstart, gs("until_on"),
                    g("interest_rule_id"), g("due_offset_days"), g("term_periods"), g("scenario_id")],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": conn.last_insert_rowid(), "pct_e15": pct }))
        }

        "interest.level_payment" => {
            let principal = params.get("principal_minor").and_then(|v| v.as_i64()).ok_or_else(|| bad("principal_minor"))?;
            let rate = params.get("periodic_rate_e15").and_then(|v| v.as_i64()).ok_or_else(|| bad("periodic_rate_e15"))?;
            let term = params.get("term_periods").and_then(|v| v.as_i64()).ok_or_else(|| bad("term_periods"))?;
            let pmt = interest::derive_level_payment(principal, rate, term)
                .map_err(|e| Error { code: "bad_rate", message: e.to_string() })?;
            Ok(serde_json::json!({ "level_payment_minor": pmt }))
        }

        // ---- CSV import ----
        // Read a file's HEADERS without staging anything, so the mapping step can offer the real
        // column names instead of asking the operator to type them from memory.
        "import.peek" => {
            let path = params.get("path").and_then(|v| v.as_str()).ok_or_else(|| bad("path"))?;
            let text = std::fs::read_to_string(path)
                .map_err(|e| Error { code: "io", message: format!("{path}: {e}") })?;
            let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(text.as_bytes());
            let headers: Vec<String> = rdr
                .headers()
                .map_err(|e| Error { code: "csv", message: e.to_string() })?
                .iter().map(|h| h.trim().to_string()).collect();
            // A few sample rows make a wrong mapping obvious before it is applied to 400 lines.
            let sample: Vec<Vec<String>> = rdr.records().take(3)
                .filter_map(|r| r.ok())
                .map(|r| r.iter().map(|c| c.to_string()).collect())
                .collect();
            Ok(serde_json::json!({ "headers": headers, "sample": sample,
                                   "total_lines": text.lines().count().saturating_sub(1) }))
        }

        "import.profiles" => {
            let mut st = conn.prepare(
                "SELECT id, name, date_format, mapping_json, account_id, default_currency
                   FROM import_profile ORDER BY name",
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rows: Vec<serde_json::Value> = st.query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?,
                    "date_format": r.get::<_, String>(2)?,
                    "mapping": serde_json::from_str::<serde_json::Value>(
                        &r.get::<_, String>(3)?).unwrap_or(serde_json::Value::Null),
                    "account_id": r.get::<_, Option<i64>>(4)?,
                    "currency": r.get::<_, String>(5)?,
                }))
            }).and_then(|m| m.collect()).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::Value::Array(rows))
        }

        "import.batches" => {
            let mut st = conn.prepare(
                "SELECT b.id, p.name, b.source_name, b.imported_at, b.row_count, b.state,
                        b.first_row_on, b.last_row_on
                   FROM import_batch b JOIN import_profile p ON p.id = b.profile_id
                  ORDER BY b.id DESC LIMIT 20",
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rows: Vec<serde_json::Value> = st.query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?, "profile": r.get::<_, String>(1)?,
                    "source": r.get::<_, String>(2)?, "imported_at": r.get::<_, String>(3)?,
                    "rows": r.get::<_, i64>(4)?, "state": r.get::<_, String>(5)?,
                    "from": r.get::<_, Option<String>>(6)?, "to": r.get::<_, Option<String>>(7)?,
                }))
            }).and_then(|m| m.collect()).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::Value::Array(rows))
        }

        "import.create_profile" => {
            let name = params.get("name").and_then(|v| v.as_str()).ok_or_else(|| bad("name"))?;
            let fmt = params.get("date_format").and_then(|v| v.as_str()).unwrap_or("%d/%m/%Y");
            let mapping = params.get("mapping").ok_or_else(|| bad("mapping"))?;
            let account = params.get("account_id").and_then(|v| v.as_i64());
            let cur = params.get("currency").and_then(|v| v.as_str()).unwrap_or("GBP");
            let delim = params.get("delimiter").and_then(|v| v.as_str()).unwrap_or(",");
            conn.execute(
                "INSERT INTO import_profile(name, date_format, mapping_json, account_id,
                    default_currency, delimiter) VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![name, fmt, mapping.to_string(), account, cur, delim],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": conn.last_insert_rowid() }))
        }

        "import.stage" => {
            let profile = params.get("profile_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("profile_id"))?;
            // Either inline text or a path -- the frontend will use a path, tests use text.
            let text = match params.get("csv").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => {
                    let path = params.get("path").and_then(|v| v.as_str())
                        .ok_or_else(|| bad("csv or path"))?;
                    std::fs::read_to_string(path)
                        .map_err(|e| Error { code: "io", message: format!("{path}: {e}") })?
                }
            };
            let name = params.get("source_name").and_then(|v| v.as_str()).unwrap_or("upload.csv");
            let rep = importer::stage(conn, profile, name, &text)?;
            Ok(serde_json::json!({
                "batch_id": rep.batch_id, "rows": rep.rows, "new": rep.new_rows,
                "duplicates": rep.duplicates, "errors": rep.errors,
            }))
        }

        "import.categorise" => {
            let b = params.get("batch_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("batch_id"))?;
            Ok(serde_json::json!({ "matched": importer::categorise(conn, b)? }))
        }
        "import.rows" => {
            let b = params.get("batch_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("batch_id"))?;
            Ok(serde_json::Value::Array(importer::rows(conn, b)?))
        }
        "import.set_row" => {
            let id = params.get("id").and_then(|v| v.as_i64()).ok_or_else(|| bad("id"))?;
            let accepted = params.get("accepted").and_then(|v| v.as_bool());
            let far = params.get("far_account_id").and_then(|v| v.as_i64());
            if let Some(a) = accepted {
                conn.execute("UPDATE import_row SET accepted=?2 WHERE id=?1",
                             rusqlite::params![id, a as i64])
                    .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            }
            if let Some(f) = far {
                conn.execute("UPDATE import_row SET far_account_id=?2 WHERE id=?1",
                             rusqlite::params![id, f])
                    .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            }
            Ok(serde_json::json!({ "id": id }))
        }
        "import.commit" => {
            let b = params.get("batch_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("batch_id"))?;
            Ok(serde_json::json!({ "created": importer::commit(conn, b)? }))
        }
        "import.revert" => {
            let b = params.get("batch_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("batch_id"))?;
            Ok(serde_json::json!({ "removed": importer::revert(conn, b)? }))
        }
        "import.create_rule" => {
            let g = |k: &str| params.get(k).and_then(|v| v.as_i64());
            let gs = |k: &str| params.get(k).and_then(|v| v.as_str());
            conn.execute(
                "INSERT INTO import_rule(name, priority, field, op, pattern, sign, set_far_account_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    gs("name").ok_or_else(|| bad("name"))?,
                    g("priority").unwrap_or(100),
                    gs("field").unwrap_or("description"),
                    gs("op").unwrap_or("contains"),
                    gs("pattern").ok_or_else(|| bad("pattern"))?,
                    g("sign").unwrap_or(0),
                    g("set_far_account_id")],
            ).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::json!({ "id": conn.last_insert_rowid() }))
        }

        // ---- export (req 7) ----
        "export.bundle" => {
            let opt = export::Options {
                from: params.get("from").and_then(|v| v.as_str()),
                to: params.get("to").and_then(|v| v.as_str()),
                redact: params.get("redact").and_then(|v| v.as_bool()).unwrap_or(false),
                forecast_to: params.get("forecast_to").and_then(|v| v.as_str()),
            };
            let doc = export::bundle(conn, &opt)?;
            match params.get("path").and_then(|v| v.as_str()) {
                Some(path) => {
                    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default())
                        .map_err(|e| Error { code: "io", message: format!("{path}: {e}") })?;
                    Ok(serde_json::json!({ "written": path,
                        "lines": doc["lines"].as_array().map(|a| a.len()).unwrap_or(0) }))
                }
                None => Ok(doc),
            }
        }

        "export.csv" => {
            let opt = export::Options {
                from: params.get("from").and_then(|v| v.as_str()),
                to: params.get("to").and_then(|v| v.as_str()),
                redact: params.get("redact").and_then(|v| v.as_bool()).unwrap_or(false),
                forecast_to: None,
            };
            let text = export::csv(conn, &opt)?;
            match params.get("path").and_then(|v| v.as_str()) {
                Some(path) => {
                    std::fs::write(path, &text)
                        .map_err(|e| Error { code: "io", message: format!("{path}: {e}") })?;
                    Ok(serde_json::json!({ "written": path, "rows": text.lines().count() - 1 }))
                }
                None => Ok(serde_json::json!({ "csv": text })),
            }
        }

        // Emptying the book — the most destructive thing this program can do, so it is built to
        // be hard to do by accident and impossible to do irreversibly.
        //
        // THREE GUARDS, and the third is the one that matters. The caller must pass the exact
        // token, so a stray or replayed RPC cannot trigger it. The reply reports what it removed,
        // so a mistake is at least visible. And it takes a VACUUM INTO snapshot FIRST, refusing to
        // proceed if that fails — which turns "destroy my finances" into "set them aside", and is
        // worth more than any amount of confirmation UI.
        //
        // currency and book_meta survive: they are reference data and schema version, not her
        // finances, and re-seeding them is exactly the kind of thing that goes wrong later. Every
        // other table is emptied, enumerated from sqlite_master rather than listed here, so a
        // table added in a future migration cannot be silently left full.
        "book.reset" => {
            const TOKEN: &str = "DELETE";
            let confirm = params.get("confirm").and_then(|v| v.as_str()).unwrap_or("");
            if confirm != TOKEN {
                return Err(Error {
                    code: "not_confirmed",
                    message: format!("book.reset requires confirm = {TOKEN:?}"),
                });
            }

            let path = conn.path().ok_or_else(|| Error::internal("book has no path"))?.to_string();
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let backup = match params.get("backup").and_then(|v| v.as_str()) {
                Some(b) => b.to_string(),
                None => {
                    let dir = std::path::Path::new(&path)
                        .parent()
                        .map(|d| d.to_string_lossy().to_string())
                        .unwrap_or_else(|| ".".into());
                    format!("{dir}/book-before-reset-{stamp}.db")
                }
            };
            // Refuse rather than proceed unbacked: an unrecoverable wipe is the one outcome this
            // method exists to prevent.
            export::snapshot_db(conn, &backup)?;

            let tables: Vec<String> = {
                let mut st = conn
                    .prepare(
                        "SELECT name FROM sqlite_master
                          WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                            AND name NOT IN ('currency', 'book_meta')",
                    )
                    .map_err(|e| Error { code: "sql", message: e.to_string() })?;
                st.query_map([], |r| r.get(0))
                    .and_then(|m| m.collect())
                    .map_err(|e| Error { code: "sql", message: e.to_string() })?
            };

            let mut cleared = serde_json::Map::new();
            // Foreign keys off for the sweep: the tables reference each other, so no single order
            // is safe, and the whole graph is going anyway.
            conn.pragma_update(None, "foreign_keys", "OFF")
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let tx = conn.transaction().map_err(|e| Error { code: "sql", message: e.to_string() })?;
            for t in &tables {
                let n = tx
                    .execute(&format!("DELETE FROM \"{t}\""), [])
                    .map_err(|e| Error { code: "sql", message: format!("{t}: {e}") })?;
                if n > 0 {
                    cleared.insert(t.clone(), serde_json::json!(n));
                }
            }
            tx.commit().map_err(|e| Error { code: "sql", message: e.to_string() })?;
            conn.pragma_update(None, "foreign_keys", "ON")
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;

            Ok(serde_json::json!({ "backup": backup, "cleared": cleared }))
        }

        "export.snapshot" => {
            let path = params.get("path").and_then(|v| v.as_str()).ok_or_else(|| bad("path"))?;
            export::snapshot_db(conn, path)?;
            Ok(serde_json::json!({ "written": path }))
        }

        // ---- analysis: the surface a model reasons over (req 7) ----
        //
        // Deliberately NOT export.bundle with more fields. The bundle hands over lines and asks the
        // reader to do the arithmetic; these hand over the arithmetic. That is the difference
        // between a book being readable and an analysis being right.
        "analysis.brief" => {
            let today = chrono::Local::now().date_naive();
            let date = |k: &str, default: chrono::NaiveDate| -> Result<chrono::NaiveDate, Error> {
                match params.get(k).and_then(|v| v.as_str()) {
                    None => Ok(default),
                    Some(s) => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .map_err(|_| bad(&format!("{k} must be YYYY-MM-DD"))),
                }
            };
            let as_of = date("as_of", today)?;
            let months = params.get("months").and_then(|v| v.as_u64()).unwrap_or(6).clamp(1, 120) as u32;
            let default_horizon = as_of
                .checked_add_months(chrono::Months::new(6))
                .unwrap_or(as_of);
            let opt = analysis::BriefOptions {
                as_of,
                months,
                horizon: date("horizon", default_horizon)?,
                largest_n: params.get("largest").and_then(|v| v.as_u64()).unwrap_or(10).clamp(0, 100) as usize,
            };
            let doc = analysis::brief(conn, &opt)?;
            // Writing it out is the common case: the brief is what gets handed to a model, and a
            // path saves the operator a copy-paste of several hundred lines.
            match params.get("path").and_then(|v| v.as_str()) {
                Some(path) => {
                    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default())
                        .map_err(|e| Error { code: "io", message: format!("{path}: {e}") })?;
                    Ok(serde_json::json!({ "written": path }))
                }
                None => Ok(doc),
            }
        }

        "analysis.query" => {
            let sql = params.get("sql").and_then(|v| v.as_str()).ok_or_else(|| bad("sql"))?;
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
            // A second, read-only connection to the same file. `conn` is the writer and must never
            // be handed a statement from outside.
            let path = conn.path().ok_or_else(|| Error::internal("book has no path"))?.to_string();
            Ok(analysis::query(&path, sql, limit)?)
        }

        "analysis.schema" => Ok(analysis::schema(conn)?),

        "analysis.tools" => Ok(analysis::tools()),

        // The payment browser. Distinct from txn.list, which stays cheap for the importer.
        "txn.browse" => {
            let g = |k: &str| params.get(k).and_then(|v| v.as_str());
            Ok(entry::browse(
                conn,
                g("search").filter(|s| !s.trim().is_empty()),
                g("from"),
                g("to"),
                params.get("account_id").and_then(|v| v.as_i64()),
                params.get("limit").and_then(|v| v.as_i64()).unwrap_or(50).clamp(1, 500),
                params.get("offset").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
            )?)
        }

        // ---- free-form links between payments (req 10, graph form) ----
        //
        // Distinct from journeys on purpose: a journey is an ordered container you plan, this is
        // an assertion between two payments made after the fact. Neither replaces the other.
        "link.create" => {
            let from = params.get("from_txn").and_then(|v| v.as_i64()).ok_or_else(|| bad("from_txn"))?;
            let to = params.get("to_txn").and_then(|v| v.as_i64()).ok_or_else(|| bad("to_txn"))?;
            let note = params.get("note").and_then(|v| v.as_str());
            let on = params
                .get("on")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| chrono::Local::now().date_naive().to_string());
            link::create(conn, from, to, note, &on)?;
            Ok(serde_json::json!({ "from": from, "to": to }))
        }

        "link.delete" => {
            let a = params.get("from_txn").and_then(|v| v.as_i64()).ok_or_else(|| bad("from_txn"))?;
            let b = params.get("to_txn").and_then(|v| v.as_i64()).ok_or_else(|| bad("to_txn"))?;
            Ok(serde_json::json!({ "removed": link::remove(conn, a, b)? }))
        }

        "link.for_txn" => {
            let id = params.get("txn_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("txn_id"))?;
            Ok(link::for_txn(conn, id)?)
        }

        // The whole connected component -- "follow the thread" -- from wherever you start.
        "link.chain" => {
            let id = params.get("txn_id").and_then(|v| v.as_i64()).ok_or_else(|| bad("txn_id"))?;
            let max = params.get("max_nodes").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
            Ok(link::chain(conn, id, max.clamp(1, 2000))?)
        }

        other => Err(Error::unknown_method(other)),
    }
}
