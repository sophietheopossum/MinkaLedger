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
                    "SELECT id, name, kind, currency, parent_id, system, closed
                       FROM account ORDER BY kind, name",
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
                        (SELECT COUNT(*) FROM series_posting sp WHERE sp.series_id = s.id)
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
                }))
            }).and_then(|m| m.collect()).map_err(|e| Error { code: "sql", message: e.to_string() })?;
            Ok(serde_json::Value::Array(rows))
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

        "scenario.list" => {
            let mut stmt = conn.prepare("SELECT id, name, note FROM scenario ORDER BY id")
                .map_err(|e| Error { code: "sql", message: e.to_string() })?;
            let rows: Vec<serde_json::Value> = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?,
                    "note": r.get::<_, Option<String>>(2)?,
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

        "export.snapshot" => {
            let path = params.get("path").and_then(|v| v.as_str()).ok_or_else(|| bad("path"))?;
            export::snapshot_db(conn, path)?;
            Ok(serde_json::json!({ "written": path }))
        }

        other => Err(Error::unknown_method(other)),
    }
}
