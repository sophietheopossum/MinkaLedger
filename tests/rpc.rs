//! End-to-end tests over the real NDJSON interface and a real database file.
//!
//! WHY THESE EXIST SEPARATELY. The unit tests build a `Snapshot` in memory and call the projection
//! directly, which is the right way to test the maths -- but it bypasses SQL entirely, so every
//! CHECK constraint in the schema goes unexercised. That gap bit for real: `payment.create_rule`
//! with `amount_kind = 'pct_of_statement'` was rejected by a constraint requiring
//! `interest_rule_id` and `due_offset_days`, the unit tests passed anyway because they never
//! touched the table, and the failure only surfaced when a demo showed a payment total of zero.
//!
//! So these drive the binary the way the QML frontend will: a pipe, one JSON object per line.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// Tests run in parallel and share a pid, so the book path needs a per-test counter as well.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// Feed `lines` to the binary against a fresh book, and return one parsed reply per response line.
fn run(lines: &[&str]) -> Vec<serde_json::Value> {
    let dir = std::env::temp_dir().join(format!("minka-ledger-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let db = dir.join(format!("book-{}.db", SEQ.fetch_add(1, Ordering::SeqCst)));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_minka-ledger"))
        .arg("--db")
        .arg(&db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn minka-ledger");
    {
        let stdin = child.stdin.as_mut().unwrap();
        for l in lines {
            // The test source wraps its JSON across several lines for readability, but the protocol
            // is one object PER LINE -- sending it as written would have each fragment ignored as
            // malformed. Collapse to a single line before writing.
            let one_line: String = l.split_whitespace().collect::<Vec<_>>().join(" ");
            writeln!(stdin, "{one_line}").unwrap();
        }
    }
    let out = child.wait_with_output().expect("run to completion");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each reply is JSON"))
        .collect()
}

fn err_of(v: &serde_json::Value) -> Option<String> {
    v.get("error")?.get("message")?.as_str().map(|s| s.to_string())
}

#[test]
fn a_statement_based_payment_rule_needs_its_statement_source() {
    // The regression this file exists for. The schema requires interest_rule_id + due_offset_days
    // for any payment sized from a statement -- without them there is no statement to be a
    // percentage OF, and the rule would silently pay nothing.
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Card","kind":"liability"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":3,"method":"payment.create_rule","params":{"account_id":1,"from_account_id":2,
             "amount_kind":"pct_of_statement","pct":"1.0","rrule":"FREQ=MONTHLY;BYMONTHDAY=15",
             "dtstart":"2026-01-15"}}"#,
    ]);
    let msg = err_of(&out[2]).expect("must be refused, not silently stored");
    assert!(msg.contains("interest_rule_id"), "the error must name what is missing: {msg}");
}

#[test]
fn a_complete_card_setup_projects_payments_that_actually_happen() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Card","kind":"liability"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Interest","kind":"expense"}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-01-01","description":"b/f",
             "postings":[{"account_id":2,"amount_minor":-200000},{"account_id":3,"amount_minor":200000}]}}"#,
        r#"{"id":5,"method":"interest.create_rule","params":{"account_id":2,"counter_account_id":3,
             "shape":"revolving","quoted_rate":"24.9","accrual_freq":"daily",
             "capitalise_dtstart":"2026-01-01","grace_period":true}}"#,
        r#"{"id":6,"method":"payment.create_rule","params":{"account_id":2,"from_account_id":1,
             "amount_kind":"pct_of_statement","pct":"1.0","floor_minor":500,
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=15","dtstart":"2026-01-15",
             "interest_rule_id":1,"due_offset_days":21}}"#,
        r#"{"id":7,"method":"forecast.project","params":{"as_of":"2026-01-01","horizon":"2026-12-31"}}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "no step may fail: {r}");
    }
    let proj = out.last().unwrap()["result"].clone();
    let occ = proj["occurrences"].as_array().unwrap();

    let payments: Vec<&serde_json::Value> =
        occ.iter().filter(|o| o["description"] == "Payment" && o["account_id"] == 2).collect();
    assert_eq!(payments.len(), 12, "twelve monthly payments, not zero");

    let interest: i64 = occ
        .iter()
        .filter(|o| o["account_id"] == 2 && o["description"].as_str().unwrap_or("").starts_with("Interest"))
        .map(|o| o["amount_minor"].as_i64().unwrap())
        .sum();
    assert!(interest < 0, "interest on a debt increases what is owed");

    // The minimum-payment trap: 1% of the statement does not keep up with 24.9%, so the debt grows.
    let closing = proj["balances"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["account_id"] == 2)
        .next_back()
        .unwrap()["balance_minor"]
        .as_i64()
        .unwrap();
    assert!(closing < -200_000, "paying only the minimum must leave MORE owed: got {closing}");
}

#[test]
fn an_unbalanced_transaction_is_refused_over_the_wire() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"A","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"B","kind":"income"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-01-01","description":"x",
             "postings":[{"account_id":1,"amount_minor":100},{"account_id":2,"amount_minor":-99}]}}"#,
        r#"{"id":4,"method":"txn.list"}"#,
    ]);
    assert_eq!(out[2]["error"]["code"], "unbalanced");
    assert!(err_of(&out[2]).unwrap().contains("residual"), "the error names the shortfall");
    assert_eq!(out[3]["result"].as_array().unwrap().len(), 0, "and nothing was written");
}

#[test]
fn the_protocol_behaves_at_its_edges() {
    let out = run(&[
        r#"{"id":1,"method":"health.ping"}"#,
        r#"{"method":"health.ping"}"#, // fire-and-forget: no reply
        r#"not json at all"#,          // ignored, not fatal
        r#"{"id":2,"method":"no.such.method"}"#,
        r#"{"id":3,"method":"db.check"}"#,
    ]);
    assert_eq!(out.len(), 3, "one reply each for ids 1, 2 and 3 only: {out:?}");
    assert_eq!(out[0]["id"], 1);
    assert_eq!(out[1]["error"]["code"], "unknown_method");
    assert_eq!(out[2]["result"]["ok"], true, "a fresh book is internally consistent");
}

#[test]
fn a_multi_currency_conversion_keeps_the_book_consistent() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Euro pot","kind":"asset","currency":"EUR"}}"#,
        r#"{"id":3,"method":"txn.convert","params":{"occurred_on":"2026-08-01","description":"holiday money",
             "from_account":1,"from_minor":40000,"to_account":2,"to_minor":46664}}"#,
        r#"{"id":4,"method":"db.check"}"#,
        r#"{"id":5,"method":"account.balances"}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }
    assert_eq!(out[3]["result"]["ok"], true, "both currencies balance: {}", out[3]);
    let bals = out[4]["result"].as_array().unwrap();
    let eur = bals.iter().find(|b| b["name"] == "Euro pot").unwrap();
    assert_eq!(eur["balance_minor"], 46664);
}
