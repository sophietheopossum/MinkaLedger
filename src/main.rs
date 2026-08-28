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
mod entry;
mod forecast;
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

        other => Err(Error::unknown_method(other)),
    }
}
