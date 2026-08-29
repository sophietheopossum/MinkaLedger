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

/// The analysis surface is the one an agent drives without a human watching, so the end-to-end
/// path matters more here than anywhere: a read-only guarantee that only holds in a unit test is
/// not a guarantee.
#[test]
fn the_brief_computes_what_a_reader_would_otherwise_get_wrong() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Salary","kind":"income","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Gym","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-07-01","description":"Salary",
             "postings":[{"account_id":1,"amount_minor":250000},{"account_id":2,"amount_minor":-250000}]}}"#,
        // 30.00 every four weeks: the annualisation trap, end to end.
        r#"{"id":5,"method":"series.create","params":{"description":"Gym","rrule":"FREQ=WEEKLY;INTERVAL=4;BYDAY=MO",
             "dtstart":"2026-01-05","postings":[{"account_id":1,"amount_minor":-3000,"role":"primary"},
                                                {"account_id":3,"amount_minor":3000,"role":"balancing"}]}}"#,
        r#"{"id":6,"method":"analysis.brief","params":{"as_of":"2026-08-29","months":6}}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }
    let b = &out[5]["result"];
    assert_eq!(b["history_window"]["to"], "2026-07-31", "August is in progress and must be excluded");

    let gym = &b["commitments"]["series"][0];
    assert_eq!(gym["occurrences_next_12m"], 13);
    assert_eq!(gym["monthly_equivalent_decimal"], "-32.50", "x12 would say -30.00 and be 8% under");

    // The month a salary landed reports it as a positive magnitude, not the ledger's negative.
    let july = b["months"].as_array().unwrap().iter().find(|m| m["month"] == "2026-07").unwrap();
    assert_eq!(july["totals"][0]["received_decimal"], "2500.00");

    let sal = b["typical"]["accounts"].as_array().unwrap().iter()
        .find(|a| a["account"] == "Salary").unwrap();
    assert_eq!(sal["direction"], "money in", "a -2500.00 median must not read as a loss");
}

#[test]
fn query_is_read_only_against_the_real_book_file() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Shop","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-07-01","description":"Tea",
             "postings":[{"account_id":1,"amount_minor":-250},{"account_id":2,"amount_minor":250}]}}"#,
        r#"{"id":4,"method":"analysis.query","params":{"sql":"SELECT account, amount_decimal FROM v_ledger_line WHERE account_kind='expense'"}}"#,
        r#"{"id":5,"method":"analysis.query","params":{"sql":"DELETE FROM txn"}}"#,
        r#"{"id":6,"method":"analysis.query","params":{"sql":"UPDATE account SET name='x'"}}"#,
        // The writer connection must still work afterwards: a refused query must not wedge it.
        r#"{"id":7,"method":"txn.create","params":{"occurred_on":"2026-07-02","description":"Coffee",
             "postings":[{"account_id":1,"amount_minor":-300},{"account_id":2,"amount_minor":300}]}}"#,
        r#"{"id":8,"method":"analysis.query","params":{"sql":"SELECT COUNT(*) FROM txn"}}"#,
        r#"{"id":9,"method":"db.check"}"#,
    ]);
    assert_eq!(out[3]["result"]["rows"][0][1], "2.50");
    assert_eq!(out[4]["error"]["code"], "bad_query");
    assert!(out[4]["error"]["message"].as_str().unwrap().contains("readonly"), "{}", out[4]);
    assert_eq!(out[5]["error"]["code"], "bad_query");
    assert!(err_of(&out[6]).is_none(), "the writer survived a refused query: {}", out[6]);
    assert_eq!(out[7]["result"]["rows"][0][0], 2, "both transactions are there, none deleted");
    assert_eq!(out[8]["result"]["ok"], true);
}

#[test]
fn an_agent_can_discover_the_surface_without_being_told_about_it() {
    let out = run(&[
        r#"{"id":1,"method":"analysis.tools"}"#,
        r#"{"id":2,"method":"analysis.schema"}"#,
    ]);
    let names: Vec<&str> = out[0]["result"]["tools"].as_array().unwrap().iter()
        .map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"analysis.brief") && names.contains(&"analysis.query"), "{names:?}");

    let objects = out[1]["result"]["objects"].as_array().unwrap();
    assert!(objects.iter().any(|o| o["name"] == "v_ledger_line"));
    // Every method the tool list advertises must actually dispatch.
    let follow: Vec<String> = names.iter()
        .map(|n| format!(r#"{{"id":1,"method":"{n}","params":{{"as_of":"2026-08-29","horizon":"2027-01-01","sql":"SELECT 1"}}}}"#))
        .collect();
    let refs: Vec<&str> = follow.iter().map(String::as_str).collect();
    for (name, reply) in names.iter().zip(run(&refs)) {
        assert_ne!(reply["error"]["code"], "unknown_method", "{name} is advertised but absent");
    }
}
