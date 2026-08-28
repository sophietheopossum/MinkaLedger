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
mod entry;
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

        other => Err(Error::unknown_method(other)),
    }
}
