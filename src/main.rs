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

mod money;

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(line) {
            let _ = writeln!(out, "{reply}");
            let _ = out.flush();
        }
    }
}

fn handle_line(line: &str) -> Option<String> {
    let msg: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None, // malformed lines are ignored, as ShojiClient does
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);

    // Catch a panic so one bad request cannot end the session.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(method, params)));
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

fn dispatch(method: &str, params: serde_json::Value) -> Result<serde_json::Value, Error> {
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

        other => Err(Error::unknown_method(other)),
    }
}
