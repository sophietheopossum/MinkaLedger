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

/// Scenarios are the feature most likely to be got wrong in a way nobody notices: a "what if"
/// that quietly alters the real book would be worse than no feature at all. So this drives the
/// whole shape end to end -- add, cancel, activate, delete -- and checks history each time.
#[test]
fn a_scenario_changes_the_forecast_and_never_the_book() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Netflix","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"series.create","params":{"description":"Netflix","rrule":"FREQ=MONTHLY;BYMONTHDAY=5",
             "dtstart":"2026-01-05","postings":[{"account_id":1,"amount_minor":-1099,"role":"primary"},
                                                {"account_id":2,"amount_minor":1099,"role":"balancing"}]}}"#,
        r#"{"id":4,"method":"scenario.create","params":{"name":"Cancel Netflix"}}"#,
        // The cancel row: scenario-scoped, supersedes the baseline, and moves no money itself.
        r#"{"id":5,"method":"series.create","params":{"description":"cancelled: Netflix",
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=5","dtstart":"2026-09-01",
             "scenario_id":1,"supersedes_id":1,"postings":[]}}"#,
        r#"{"id":6,"method":"forecast.project","params":{"as_of":"2026-08-29","horizon":"2026-12-31","scenarios":[]}}"#,
        r#"{"id":7,"method":"forecast.project","params":{"as_of":"2026-08-29","horizon":"2026-12-31","scenarios":[1]}}"#,
        r#"{"id":8,"method":"scenario.list"}"#,
        r#"{"id":9,"method":"series.list"}"#,
        r#"{"id":10,"method":"txn.list","params":{}}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }
    let baseline = out[5]["result"]["occurrences"].as_array().unwrap().len();
    let whatif = out[6]["result"]["occurrences"].as_array().unwrap().len();
    assert!(baseline > 0, "baseline should project Netflix");
    assert_eq!(whatif, 0, "the scenario cancels it, so nothing is projected");

    let sc = &out[7]["result"][0];
    assert_eq!(sc["series_count"], 1);
    assert_eq!(sc["supersedes_count"], 1, "a cancellation must be visible as one");

    // The cancel row names what it cancels, so the UI never has to show a bare id.
    let cancel = out[8]["result"].as_array().unwrap().iter()
        .find(|s| s["scenario_id"] == 1).unwrap();
    assert_eq!(cancel["supersedes"], "Netflix");
    assert_eq!(cancel["postings"], 0, "a cancellation moves no money");
    assert!(cancel["amount_minor"].is_null());

    // Nothing about any of this touched history.
    assert!(out[9]["result"].as_array().unwrap().is_empty(), "a scenario must write no transactions");
}

#[test]
fn deleting_a_scenario_takes_its_changes_and_restores_the_baseline() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Netflix","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"series.create","params":{"description":"Netflix","rrule":"FREQ=MONTHLY;BYMONTHDAY=5",
             "dtstart":"2026-01-05","postings":[{"account_id":1,"amount_minor":-1099,"role":"primary"},
                                                {"account_id":2,"amount_minor":1099,"role":"balancing"}]}}"#,
        r#"{"id":4,"method":"scenario.create","params":{"name":"Cancel Netflix"}}"#,
        r#"{"id":5,"method":"series.create","params":{"description":"cancelled: Netflix",
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=5","dtstart":"2026-09-01",
             "scenario_id":1,"supersedes_id":1,"postings":[]}}"#,
        r#"{"id":6,"method":"scenario.delete","params":{"id":1}}"#,
        r#"{"id":7,"method":"series.list"}"#,
        r#"{"id":8,"method":"forecast.project","params":{"as_of":"2026-08-29","horizon":"2026-12-31","scenarios":[]}}"#,
        r#"{"id":9,"method":"scenario.delete","params":{"id":99}}"#,
        r#"{"id":10,"method":"db.check"}"#,
    ]);
    assert_eq!(out[5]["result"]["deleted"], 1);

    // CASCADE removed the scenario's series; the baseline one is untouched.
    let series = out[6]["result"].as_array().unwrap();
    assert_eq!(series.len(), 1, "only the baseline series should remain: {series:?}");
    assert_eq!(series[0]["description"], "Netflix");
    assert!(series[0]["scenario_id"].is_null());

    assert!(!out[7]["result"]["occurrences"].as_array().unwrap().is_empty(),
            "with the scenario gone the baseline projects again");
    assert_eq!(out[8]["error"]["code"], "not_found", "deleting a missing scenario is an error, not a silent no-op");
    assert_eq!(out[9]["result"]["ok"], true);
}

/// Removing an account is the one destructive thing the GUI offers, and the wrong answer is
/// unrecoverable: a posting is half of a transaction, so deleting an account with history would
/// unbalance the book permanently. The core must therefore refuse, and say why.
#[test]
fn an_account_with_history_cannot_be_deleted_only_hidden() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Rent","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Typo","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"Rent",
             "postings":[{"account_id":1,"amount_minor":-90000},{"account_id":2,"amount_minor":90000}]}}"#,
        r#"{"id":5,"method":"account.delete","params":{"id":2}}"#,
        r#"{"id":6,"method":"account.delete","params":{"id":3}}"#,
        r#"{"id":7,"method":"account.delete","params":{"id":404}}"#,
        r#"{"id":8,"method":"account.close","params":{"id":2,"closed":true}}"#,
        r#"{"id":9,"method":"account.list"}"#,
        r#"{"id":10,"method":"account.balances"}"#,
        r#"{"id":11,"method":"db.check"}"#,
    ]);

    // Refused, and the message names what is in the way rather than quoting a constraint.
    assert_eq!(out[4]["error"]["code"], "in_use");
    let msg = out[4]["error"]["message"].as_str().unwrap();
    assert!(msg.contains("1 transaction"), "must say what blocks it: {msg}");
    assert!(msg.contains("close it instead"), "must say what to do instead: {msg}");

    // An account nothing refers to is a typo, and loses nothing.
    assert_eq!(out[5]["result"]["deleted"], 3);
    assert_eq!(out[6]["error"]["code"], "not_found");

    // Hidden, not gone: account.list still has it (that is the only way back), while
    // account.balances -- what the pickers use -- does not.
    let listed = out[8]["result"].as_array().unwrap();
    let rent = listed.iter().find(|a| a["name"] == "Rent").unwrap();
    assert_eq!(rent["closed"], true);
    assert_eq!(rent["postings"], 1, "the counts are what the UI uses to offer hide vs delete");
    assert!(!listed.iter().any(|a| a["name"] == "Typo"), "the deleted one is really gone");

    let open = out[9]["result"].as_array().unwrap();
    assert!(!open.iter().any(|a| a["name"] == "Rent"), "a hidden account leaves the pickers");

    assert_eq!(out[10]["result"]["ok"], true, "the book still balances after all of that");
}

#[test]
fn a_system_account_is_never_removable() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Euro pot","kind":"asset","currency":"EUR"}}"#,
        // A conversion happens: this is what creates the system trading account.
        r#"{"id":3,"method":"txn.convert","params":{"occurred_on":"2026-08-01","description":"holiday money",
             "from_account":1,"from_minor":40000,"to_account":2,"to_minor":46664}}"#,
        r#"{"id":4,"method":"account.list"}"#,
    ]);
    let sys: Vec<_> = out[3]["result"].as_array().unwrap().iter()
        .filter(|a| a["system"] == true).collect();
    assert!(!sys.is_empty(), "a conversion should have created a system account");

    let id = sys[0]["id"].as_i64().unwrap();
    let del = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Euro pot","kind":"asset","currency":"EUR"}}"#,
        r#"{"id":3,"method":"txn.convert","params":{"occurred_on":"2026-08-01","description":"holiday money",
             "from_account":1,"from_minor":40000,"to_account":2,"to_minor":46664}}"#,
        &format!(r#"{{"id":4,"method":"account.delete","params":{{"id":{id}}}}}"#),
    ]);
    assert_eq!(del[3]["error"]["code"], "system_account", "{}", del[3]);
}

/// Emptying the book is the one action that could destroy everything, so its guards are tested
/// rather than assumed -- including that the backup is a real book and not an empty file.
#[test]
fn emptying_the_book_needs_the_exact_token_and_leaves_a_restorable_copy() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Rent","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"Rent",
             "postings":[{"account_id":1,"amount_minor":-90000},{"account_id":2,"amount_minor":90000}]}}"#,
        r#"{"id":4,"method":"scenario.create","params":{"name":"Whatif"}}"#,
        r#"{"id":5,"method":"book.reset","params":{}}"#,
        r#"{"id":6,"method":"book.reset","params":{"confirm":"delete"}}"#,
        r#"{"id":7,"method":"book.reset","params":{"confirm":"DELETE "}}"#,
        r#"{"id":8,"method":"book.reset","params":{"confirm":"DELETE"}}"#,
        r#"{"id":9,"method":"account.list"}"#,
        r#"{"id":10,"method":"analysis.query","params":{"sql":"SELECT (SELECT COUNT(*) FROM txn),(SELECT COUNT(*) FROM posting),(SELECT COUNT(*) FROM currency),(SELECT COUNT(*) FROM book_meta)"}}"#,
        r#"{"id":11,"method":"db.check"}"#,
        // The book must still be usable, not just empty.
        r#"{"id":12,"method":"account.create","params":{"name":"Fresh","kind":"asset","currency":"GBP"}}"#,
    ]);

    // Anything but the exact token is refused -- including a trailing space, which is what a
    // paste or an autocomplete would leave behind.
    for i in [4usize, 5, 6] {
        assert_eq!(out[i]["error"]["code"], "not_confirmed", "{}", out[i]);
    }

    let done = &out[7]["result"];
    let backup = done["backup"].as_str().expect("a backup path must be reported");
    assert!(backup.contains("book-before-reset-"), "got {backup}");
    let cleared = done["cleared"].as_object().unwrap();
    assert_eq!(cleared["account"], 2);
    assert_eq!(cleared["txn"], 1);
    assert_eq!(cleared["posting"], 2);
    assert_eq!(cleared["scenario"], 1);

    assert!(out[8]["result"].as_array().unwrap().is_empty(), "no accounts survive");
    let row = &out[9]["result"]["rows"][0];
    assert_eq!(row[0], 0, "no transactions");
    assert_eq!(row[1], 0, "no postings");
    // Reference data and schema version are NOT her finances and must survive, or the book comes
    // back unusable and a re-seed becomes a migration problem.
    assert_eq!(row[2], 4, "the seeded currencies survive");
    assert!(row[3].as_i64().unwrap() >= 1, "book_meta survives");

    assert_eq!(out[10]["result"]["ok"], true);
    assert_eq!(out[11]["result"]["id"], 1, "the emptied book is immediately usable");

    // The backup is a real book with the data in it, not a zero-byte placeholder.
    let restored = std::process::Command::new(env!("CARGO_BIN_EXE_minka-ledger"))
        .arg("--db").arg(backup)
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn().expect("open the backup");
    {
        let mut si = restored.stdin.as_ref().unwrap();
        writeln!(si, r#"{{"id":1,"method":"account.list"}}"#).unwrap();
    }
    let done2 = restored.wait_with_output().unwrap();
    let reply: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&done2.stdout).lines().next().unwrap()).unwrap();
    assert_eq!(reply["result"].as_array().unwrap().len(), 2,
               "the backup still has both accounts: {reply}");
    let _ = std::fs::remove_file(backup);
}

/// minor_digits is the divisor for every amount in a currency, so a wrong one is a silent 100x
/// error. These guard the ways a bad value could get in.
#[test]
fn a_currency_can_be_added_but_never_with_an_unusable_scale() {
    let out = run(&[
        r#"{"id":1,"method":"currency.create","params":{"code":"chf","minor_digits":2,"name":"Swiss Franc"}}"#,
        r#"{"id":2,"method":"currency.create","params":{"code":"KWD","minor_digits":3,"name":"Kuwaiti Dinar"}}"#,
        r#"{"id":3,"method":"currency.create","params":{"code":"EURO","minor_digits":2}}"#,
        r#"{"id":4,"method":"currency.create","params":{"code":"E1H","minor_digits":2}}"#,
        r#"{"id":5,"method":"currency.create","params":{"code":"ETH","minor_digits":18}}"#,
        r#"{"id":6,"method":"currency.create","params":{"code":"XXX","minor_digits":-1}}"#,
        r#"{"id":7,"method":"currency.create","params":{"code":"GBP","minor_digits":2,"name":"dup"}}"#,
        r#"{"id":8,"method":"currency.list"}"#,
    ]);
    // Lower case is accepted and normalised: the schema wants three characters, not three
    // uppercase ones, and rejecting "chf" would be pedantry rather than safety.
    assert_eq!(out[0]["result"]["code"], "CHF");
    assert_eq!(out[1]["result"]["minor_digits"], 3);
    assert_eq!(out[2]["error"]["code"], "bad_params", "four letters");
    assert_eq!(out[3]["error"]["code"], "bad_params", "digits in the code");
    // The cap is not arbitrary: i64 minor units overflow at 9.22 units when the scale is 18.
    let eth = out[4]["error"]["message"].as_str().unwrap();
    assert!(eth.contains("0..=8") && eth.contains("overflow"), "{eth}");
    assert_eq!(out[5]["error"]["code"], "bad_params", "negative scale");
    assert_eq!(out[6]["error"]["code"], "already_exists");

    let list = out[7]["result"].as_array().unwrap();
    let gbp = list.iter().find(|c| c["code"] == "GBP").unwrap();
    assert_eq!(gbp["is_display"], true, "the UI needs this to hide a remove control that would fail");
    assert_eq!(gbp["name"], "Pound Sterling", "a refused duplicate must not have overwritten it");
}

#[test]
fn a_currency_in_use_cannot_be_removed_and_an_account_keeps_the_one_it_was_given() {
    let out = run(&[
        r#"{"id":1,"method":"currency.create","params":{"code":"CHF","minor_digits":2,"name":"Swiss Franc"}}"#,
        r#"{"id":2,"method":"currency.create","params":{"code":"KWD","minor_digits":3,"name":"Kuwaiti Dinar"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Swiss pot","kind":"asset","currency":"CHF"}}"#,
        r#"{"id":4,"method":"currency.delete","params":{"code":"CHF"}}"#,
        r#"{"id":5,"method":"currency.delete","params":{"code":"KWD"}}"#,
        r#"{"id":6,"method":"currency.delete","params":{"code":"GBP"}}"#,
        r#"{"id":7,"method":"currency.delete","params":{"code":"ZZZ"}}"#,
        r#"{"id":8,"method":"account.list"}"#,
        r#"{"id":9,"method":"currency.list"}"#,
    ]);
    assert_eq!(out[3]["error"]["code"], "in_use", "an account holds CHF");
    assert_eq!(out[4]["result"]["deleted"], "KWD", "nothing uses KWD");
    assert!(out[5]["error"]["message"].as_str().unwrap().contains("display currency"));
    assert_eq!(out[6]["error"]["code"], "not_found");

    // The account was created in the currency it was ASKED for, not the GBP default -- which is
    // the bug that made this whole feature necessary.
    let acct = out[7]["result"].as_array().unwrap().iter()
        .find(|a| a["name"] == "Swiss pot").unwrap();
    assert_eq!(acct["currency"], "CHF");

    let codes: Vec<&str> = out[8]["result"].as_array().unwrap().iter()
        .map(|c| c["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"CHF") && !codes.contains(&"KWD"), "{codes:?}");
}

/// The UI now parses and formats each amount at its own currency's scale, having previously
/// assumed 2 everywhere. This pins the core behaviour it relies on: the SAME text means different
/// integers in different currencies, and getting that wrong is a silent 100x error.
#[test]
fn the_same_text_parses_to_different_integers_per_currency_scale() {
    let out = run(&[
        r#"{"id":1,"method":"money.parse","params":{"text":"1000","minor_digits":0}}"#,
        r#"{"id":2,"method":"money.parse","params":{"text":"1000","minor_digits":2}}"#,
        r#"{"id":3,"method":"money.parse","params":{"text":"1000","minor_digits":3}}"#,
        r#"{"id":4,"method":"money.parse","params":{"text":"12.345","minor_digits":3}}"#,
        // Excess precision is refused rather than rounded away -- a JPY account cannot take 0.5.
        r#"{"id":5,"method":"money.parse","params":{"text":"0.5","minor_digits":0}}"#,
        r#"{"id":6,"method":"money.format","params":{"minor":1000,"minor_digits":0}}"#,
        r#"{"id":7,"method":"money.format","params":{"minor":1000,"minor_digits":2}}"#,
        r#"{"id":8,"method":"money.format","params":{"minor":1000,"minor_digits":3}}"#,
    ]);
    assert_eq!(out[0]["result"]["minor"], 1000, "JPY 1000 is 1000 yen");
    assert_eq!(out[1]["result"]["minor"], 100_000, "GBP 1000 is 100000 pence");
    assert_eq!(out[2]["result"]["minor"], 1_000_000, "KWD 1000 is 1000000 fils");
    assert_eq!(out[3]["result"]["minor"], 12_345);
    assert!(err_of(&out[4]).is_some(), "0.5 has no meaning in a 0dp currency");

    // And back the other way, which is what the Money singleton mirrors locally.
    assert_eq!(out[5]["result"]["formatted"], "1000");
    assert_eq!(out[6]["result"]["formatted"], "10.00");
    assert_eq!(out[7]["result"]["formatted"], "1.000");
}

/// A payment chain across three transactions and two days -- the shape the GUI renders.
///
/// The residual is the reason journeys exist, and its semantics are easy to get wrong: it is NOT
/// empty when a transfer completes. Source, destination and fee all remain; only the accounts the
/// money passed straight THROUGH fall to zero and drop out. The UI keys completion off the
/// `arrival` role for exactly this reason, so both facts are pinned here.
#[test]
fn a_payment_chain_shows_where_the_money_stopped() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Starling","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Wise","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Revolut","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":4,"method":"account.create","params":{"name":"Bank fees","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":5,"method":"txn.create","params":{"occurred_on":"2026-08-25","description":"Starling to Wise",
             "postings":[{"account_id":1,"amount_minor":-50000},{"account_id":2,"amount_minor":50000}]}}"#,
        r#"{"id":6,"method":"txn.create","params":{"occurred_on":"2026-08-25","description":"Wise fee",
             "postings":[{"account_id":2,"amount_minor":-320},{"account_id":4,"amount_minor":320}]}}"#,
        r#"{"id":7,"method":"journey.create","params":{"label":"Starling to Revolut","opened_on":"2026-08-25"}}"#,
        r#"{"id":8,"method":"journey.attach","params":{"journey_id":1,"txn_id":1,"seq":0,"role":"source"}}"#,
        r#"{"id":9,"method":"journey.attach","params":{"journey_id":1,"txn_id":2,"seq":1,"role":"fee"}}"#,
        r#"{"id":10,"method":"journey.get","params":{"id":1}}"#,
        // two days later it lands
        r#"{"id":11,"method":"txn.create","params":{"occurred_on":"2026-08-27","description":"Wise to Revolut",
             "postings":[{"account_id":2,"amount_minor":-49680},{"account_id":3,"amount_minor":49680}]}}"#,
        r#"{"id":12,"method":"journey.attach","params":{"journey_id":1,"txn_id":3,"seq":2,"role":"arrival"}}"#,
        r#"{"id":13,"method":"journey.get","params":{"id":1}}"#,
        r#"{"id":14,"method":"journey.list"}"#,
        r#"{"id":15,"method":"db.check"}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }

    // MID-FLIGHT: the money is sitting in Wise, and that is the question a chain answers.
    let mid = &out[9]["result"];
    let wise = mid["residual"].as_array().unwrap().iter()
        .find(|x| x["account"] == "Wise").expect("Wise holds the money mid-flight");
    assert_eq!(wise["amount_minor"], 49_680, "500.00 in, 3.20 fee out");

    // ARRIVED: Wise nets to zero and DROPS OUT; the endpoints and the fee do not.
    let done = &out[12]["result"];
    let accounts: Vec<&str> = done["residual"].as_array().unwrap().iter()
        .map(|x| x["account"].as_str().unwrap()).collect();
    assert!(!accounts.contains(&"Wise"), "a passed-through account nets to zero: {accounts:?}");
    assert!(accounts.contains(&"Starling") && accounts.contains(&"Revolut"),
            "endpoints stay in the residual, so 'empty means done' is wrong: {accounts:?}");
    assert!(!done["residual"].as_array().unwrap().is_empty(),
            "the residual is NEVER empty for a real transfer -- the UI must not key completion on it");

    // Ordered, and the arrival role is the completion signal the UI uses.
    let legs = done["legs"].as_array().unwrap();
    let roles: Vec<&str> = legs.iter().map(|l| l["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["source", "fee", "arrival"]);
    let seqs: Vec<i64> = legs.iter().map(|l| l["seq"].as_i64().unwrap()).collect();
    assert_eq!(seqs, vec![0, 1, 2], "seq drives the order the chain is drawn in");

    assert_eq!(out[13]["result"][0]["legs"], 3);
    assert_eq!(out[14]["result"]["ok"], true, "linking transactions writes no postings");
}

#[test]
fn attaching_the_same_step_twice_is_refused_rather_than_reordering_the_chain() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"A","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"B","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-08-25","description":"one",
             "postings":[{"account_id":1,"amount_minor":-100},{"account_id":2,"amount_minor":100}]}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-08-26","description":"two",
             "postings":[{"account_id":1,"amount_minor":-200},{"account_id":2,"amount_minor":200}]}}"#,
        r#"{"id":5,"method":"journey.create","params":{"label":"chain","opened_on":"2026-08-25"}}"#,
        r#"{"id":6,"method":"journey.attach","params":{"journey_id":1,"txn_id":1,"seq":0,"role":"source"}}"#,
        // same seq as an existing leg: UNIQUE(journey_id, seq) must reject it
        r#"{"id":7,"method":"journey.attach","params":{"journey_id":1,"txn_id":2,"seq":0,"role":"leg"}}"#,
        r#"{"id":8,"method":"journey.attach","params":{"journey_id":1,"txn_id":2,"seq":1,"role":"arrival"}}"#,
        r#"{"id":9,"method":"journey.detach","params":{"journey_id":1,"txn_id":2}}"#,
        r#"{"id":10,"method":"journey.get","params":{"id":1}}"#,
        r#"{"id":11,"method":"txn.list","params":{}}"#,
    ]);
    assert!(err_of(&out[6]).is_some(), "a duplicate seq must not silently reorder the chain");
    assert!(err_of(&out[7]).is_none(), "the next seq is fine");
    assert_eq!(out[8]["result"]["detached"], true);
    assert_eq!(out[9]["result"]["legs"].as_array().unwrap().len(), 1, "detach removed the leg");
    // Detaching a step must never touch the transaction itself.
    assert_eq!(out[10]["result"].as_array().unwrap().len(), 2, "both transactions survive");
}

/// Free-form links: any payment to any other, followed from either end.
///
/// The property that matters is that direction is RECORDED but not OBEYED when traversing —
/// starting at the last payment in a chain must still show the whole chain.
#[test]
fn payments_link_into_a_chain_followable_from_either_end() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Starling","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Wise","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Revolut","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-08-25","description":"Starling to Wise",
             "postings":[{"account_id":1,"amount_minor":-50000},{"account_id":2,"amount_minor":50000}]}}"#,
        r#"{"id":5,"method":"txn.create","params":{"occurred_on":"2026-08-26","description":"Wise fee",
             "postings":[{"account_id":2,"amount_minor":-320},{"account_id":1,"amount_minor":320}]}}"#,
        r#"{"id":6,"method":"txn.create","params":{"occurred_on":"2026-08-27","description":"Wise to Revolut",
             "postings":[{"account_id":2,"amount_minor":-49680},{"account_id":3,"amount_minor":49680}]}}"#,
        r#"{"id":7,"method":"link.create","params":{"from_txn":1,"to_txn":2,"note":"its fee"}}"#,
        r#"{"id":8,"method":"link.create","params":{"from_txn":2,"to_txn":3}}"#,
        // every way of asking for something the graph already says, or cannot say
        r#"{"id":9,"method":"link.create","params":{"from_txn":2,"to_txn":1}}"#,
        r#"{"id":10,"method":"link.create","params":{"from_txn":1,"to_txn":1}}"#,
        r#"{"id":11,"method":"link.create","params":{"from_txn":1,"to_txn":404}}"#,
        // follow from the LAST payment: direction must not limit reachability
        r#"{"id":12,"method":"link.chain","params":{"txn_id":3}}"#,
        r#"{"id":13,"method":"link.chain","params":{"txn_id":1}}"#,
        r#"{"id":14,"method":"link.for_txn","params":{"txn_id":2}}"#,
        r#"{"id":15,"method":"db.check"}"#,
    ]);
    assert!(err_of(&out[6]).is_none() && err_of(&out[7]).is_none());
    assert_eq!(out[8]["error"]["code"], "already_linked", "the reverse edge is the same assertion");
    assert_eq!(out[9]["error"]["code"], "self_link");
    assert_eq!(out[10]["error"]["code"], "no_such_txn");

    // From the far end, the whole chain, with hop counts measured from where you started.
    let from_end = &out[11]["result"];
    assert_eq!(from_end["nodes"].as_array().unwrap().len(), 3, "{from_end}");
    assert_eq!(from_end["edges"].as_array().unwrap().len(), 2);
    let d = |v: &serde_json::Value, id: i64| -> i64 {
        v["nodes"].as_array().unwrap().iter()
            .find(|n| n["id"] == id).unwrap()["depth"].as_i64().unwrap()
    };
    assert_eq!((d(from_end, 3), d(from_end, 2), d(from_end, 1)), (0, 1, 2));

    // ...and from the other end the same set, with the depths mirrored.
    let from_start = &out[12]["result"];
    assert_eq!(from_start["nodes"].as_array().unwrap().len(), 3);
    assert_eq!((d(from_start, 1), d(from_start, 2), d(from_start, 3)), (0, 1, 2));

    // The middle payment reports both directions, and the note survives.
    let linked = out[13]["result"]["linked"].as_array().unwrap();
    assert_eq!(linked.len(), 2);
    let inbound = linked.iter().find(|l| l["id"] == 1).unwrap();
    assert_eq!(inbound["direction"], "in");
    assert_eq!(inbound["note"], "its fee");
    assert_eq!(linked.iter().find(|l| l["id"] == 3).unwrap()["direction"], "out");

    assert_eq!(out[14]["result"]["ok"], true, "linking writes no postings");

    // The residual is what the chain is FOR: where the money ended up. Wise handed on everything
    // it received, so it nets to zero and is absent -- that absence is the signal, and it is why
    // "residual is empty" can never mean "finished".
    let net = from_end["residual"].as_array().unwrap();
    let named: Vec<&str> = net.iter().map(|x| x["account"].as_str().unwrap()).collect();
    assert!(!named.contains(&"Wise"), "a passed-through account nets to zero: {named:?}");
    assert!(named.contains(&"Starling") && named.contains(&"Revolut"), "{named:?}");
    let starling = net.iter().find(|x| x["account"] == "Starling").unwrap();
    assert_eq!(starling["amount_minor"], -49_680, "500.00 out, 3.20 of fee refunded to it");
}

#[test]
fn deleting_a_payment_takes_its_links_and_browse_finds_what_you_search_for() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Shop","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-08-25","description":"TESCO run",
             "payee":"Tesco","postings":[{"account_id":1,"amount_minor":-1000},{"account_id":2,"amount_minor":1000}]}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-08-26","description":"refund",
             "postings":[{"account_id":1,"amount_minor":500},{"account_id":2,"amount_minor":-500}]}}"#,
        r#"{"id":5,"method":"link.create","params":{"from_txn":1,"to_txn":2}}"#,
        r#"{"id":6,"method":"txn.browse","params":{"search":"tesco"}}"#,
        r#"{"id":7,"method":"txn.browse","params":{"search":"TESCO"}}"#,
        r#"{"id":8,"method":"txn.browse","params":{"account_id":2}}"#,
        r#"{"id":9,"method":"txn.delete","params":{"id":2}}"#,
        r#"{"id":10,"method":"link.chain","params":{"txn_id":1}}"#,
        r#"{"id":11,"method":"db.check"}"#,
    ]);
    // Search covers the payee too, and is case-insensitive both ways round.
    assert_eq!(out[5]["result"]["total"], 1, "matched on payee");
    assert_eq!(out[6]["result"]["total"], 1, "upper case finds the lower-case description");
    assert_eq!(out[7]["result"]["total"], 2, "both touch the Shop account");

    let row = &out[5]["result"]["rows"][0];
    assert_eq!(row["links"], 1, "the browser shows what is already threaded");
    assert_eq!(row["postings"].as_array().unwrap().len(), 2, "a row carries its legs");

    // A link is a statement ABOUT two payments and cannot outlive either.
    assert!(err_of(&out[8]).is_none(), "{}", out[8]);
    let left = &out[9]["result"];
    assert_eq!(left["nodes"].as_array().unwrap().len(), 1, "only the survivor remains");
    assert!(left["edges"].as_array().unwrap().is_empty(), "the dangling edge went with it");
    assert_eq!(out[10]["result"]["ok"], true);
}

/// Most recurring payments do not recur forever, and an unbounded rule projects into every
/// forecast you ever draw. until_on is INCLUSIVE (RFC 5545 3.3.10): the occurrence landing
/// exactly on it still happens.
#[test]
fn a_recurring_payment_can_be_bounded_after_it_was_created() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Bank","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Gym","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"series.create","params":{"description":"Gym","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-01-01","postings":[{"account_id":1,"amount_minor":-3000,"role":"primary"},
                                                {"account_id":2,"amount_minor":3000,"role":"balancing"}]}}"#,
        r#"{"id":4,"method":"forecast.project","params":{"as_of":"2026-08-31","horizon":"2027-06-30"}}"#,
        r#"{"id":5,"method":"series.end","params":{"id":1,"until_on":"2026-12-01"}}"#,
        r#"{"id":6,"method":"forecast.project","params":{"as_of":"2026-08-31","horizon":"2027-06-30"}}"#,
        r#"{"id":7,"method":"series.end","params":{"id":1,"until_on":"2025-01-01"}}"#,
        r#"{"id":8,"method":"series.end","params":{"id":1,"until_on":"not-a-date"}}"#,
        r#"{"id":9,"method":"series.end","params":{"id":404,"until_on":"2026-12-01"}}"#,
        r#"{"id":10,"method":"series.end","params":{"id":1}}"#,
        r#"{"id":11,"method":"forecast.project","params":{"as_of":"2026-08-31","horizon":"2027-06-30"}}"#,
        r#"{"id":12,"method":"series.list"}"#,
    ]);
    let outgoing = |v: &serde_json::Value| -> Vec<String> {
        v["result"]["occurrences"].as_array().unwrap().iter()
            .filter(|o| o["amount_minor"].as_i64().unwrap() < 0)
            .map(|o| o["value_on"].as_str().unwrap().to_string())
            .collect()
    };
    let before = outgoing(&out[3]);
    assert_eq!(before.len(), 10, "unbounded, it runs to the horizon");

    assert_eq!(out[4]["result"]["until_on"], "2026-12-01");
    let after = outgoing(&out[5]);
    assert_eq!(after.len(), 4);
    assert_eq!(after.last().unwrap(), "2026-12-01",
               "until_on is INCLUSIVE -- the occurrence on it still happens");

    // Ending before it starts is refused in those words, not as a constraint name.
    assert!(out[6]["error"]["message"].as_str().unwrap().contains("before it starts"));
    assert_eq!(out[7]["error"]["code"], "bad_params");
    assert_eq!(out[8]["error"]["code"], "not_found");

    // ...and it can be unbounded again.
    assert!(out[9]["result"]["until_on"].is_null());
    assert_eq!(outgoing(&out[10]).len(), 10, "clearing restores the endless rule");

    // The list carries what the UI needs to show and edit it.
    let s = &out[11]["result"][0];
    assert!(s["until_on"].is_null());
    assert_eq!(s["currency"], "GBP", "the amount must not be assumed to be 2dp GBP");
    assert_eq!(s["amount_minor"], -3000);
}

/// A starting balance cannot appear from nowhere: every posting needs a counterpart or the book
/// stops balancing. account.create writes the conventional equity pair so nobody has to know that.
#[test]
fn an_account_can_start_with_a_balance_and_the_book_still_balances() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP",
             "opening_minor":125000,"opening_on":"2026-08-01"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Credit card","kind":"liability","currency":"GBP",
             "opening_minor":-43000,"opening_on":"2026-08-01"}}"#,
        // EUR is one of the four seeded currencies, so it needs no creating.
        r#"{"id":3,"method":"account.create","params":{"name":"Euro pot","kind":"asset","currency":"EUR",
             "opening_minor":50000,"opening_on":"2026-08-01"}}"#,
        // no opening balance at all, and an explicit zero: neither should write anything
        r#"{"id":4,"method":"account.create","params":{"name":"Groceries","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":5,"method":"account.create","params":{"name":"Books","kind":"expense","currency":"GBP","opening_minor":0}}"#,
        r#"{"id":6,"method":"account.balances"}"#,
        r#"{"id":7,"method":"txn.browse","params":{"limit":10}}"#,
        r#"{"id":8,"method":"db.check"}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }
    let bal = |name: &str| -> i64 {
        out[5]["result"].as_array().unwrap().iter()
            .find(|b| b["name"] == name).unwrap_or_else(|| panic!("no {name}"))["balance_minor"]
            .as_i64().unwrap()
    };
    assert_eq!(bal("Current"), 125_000);
    assert_eq!(bal("Credit card"), -43_000, "a liability starts negative -- you owe it");
    assert_eq!(bal("Euro pot"), 50_000);

    // ONE equity account per currency: a transaction balances per currency, and the composite FK
    // ties a posting's currency to its account's, so a single shared one could not work.
    assert_eq!(bal("Opening balances (GBP)"), -82_000, "1250.00 asset less 430.00 owed");
    assert_eq!(bal("Opening balances (EUR)"), -50_000);

    // Only three opening transactions -- the two accounts without a balance wrote nothing.
    let rows = out[6]["result"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "an absent or zero opening balance must not write a transaction");
    assert!(rows.iter().all(|t| t["description"].as_str().unwrap().starts_with("Opening balance:")));

    assert_eq!(out[7]["result"]["ok"], true, "the whole point: it still balances");
}

/// An opening balance is a figure you refine as real history arrives, so it has to be editable
/// after the fact -- amount and date both.
#[test]
fn an_opening_balance_can_be_changed_added_and_removed_afterwards() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP",
             "opening_minor":125000,"opening_on":"2026-08-01"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Savings","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.list"}"#,
        r#"{"id":4,"method":"account.opening","params":{"id":1}}"#,
        // move both the amount and the date
        r#"{"id":5,"method":"account.set_opening","params":{"id":1,"amount_minor":98750,"occurred_on":"2026-01-01"}}"#,
        r#"{"id":6,"method":"account.opening","params":{"id":1}}"#,
        r#"{"id":7,"method":"db.check"}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }
    let opening = &out[3]["result"];
    assert_eq!(opening["amount_minor"], 125_000);
    assert_eq!(opening["occurred_on"], "2026-08-01");
    let original_txn = opening["txn_id"].as_i64().unwrap();

    let moved = &out[5]["result"];
    assert_eq!(moved["amount_minor"], 98_750);
    assert_eq!(moved["occurred_on"], "2026-01-01");
    // Updated IN PLACE. A delete-and-rewrite would silently drop any link the payment browser has
    // made to it, since txn_link cascades with the transaction.
    assert_eq!(moved["txn_id"].as_i64().unwrap(), original_txn, "the transaction id must survive");
    assert_eq!(out[4]["result"]["updated"], true);
    assert_eq!(out[6]["result"]["ok"], true);
}

#[test]
fn the_equity_counterweight_cannot_itself_be_given_an_opening_balance() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP",
             "opening_minor":125000,"opening_on":"2026-08-01"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Savings","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.list"}"#,
    ]);
    let equity = out[2]["result"].as_array().unwrap().iter()
        .find(|a| a["kind"] == "equity").expect("the counterweight was created");
    let eq_id = equity["id"].as_i64().unwrap();
    let savings = out[2]["result"].as_array().unwrap().iter()
        .find(|a| a["name"] == "Savings").unwrap()["id"].as_i64().unwrap();

    let out2 = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP",
             "opening_minor":125000,"opening_on":"2026-08-01"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Savings","kind":"asset","currency":"GBP"}}"#,
        // Both legs would target the same account, the second UPDATE would overwrite the first,
        // and the transaction would be left unbalanced. Caught by a test that did exactly that.
        &format!(r#"{{"id":3,"method":"account.set_opening","params":{{"id":{eq_id},"amount_minor":5000}}}}"#),
        &format!(r#"{{"id":4,"method":"account.set_opening","params":{{"id":{savings},"amount_minor":300000,"occurred_on":"2026-01-01"}}}}"#),
        r#"{"id":5,"method":"account.balances"}"#,
        r#"{"id":6,"method":"db.check"}"#,
        // and removing one takes the transaction with it rather than leaving zero postings
        &format!(r#"{{"id":7,"method":"account.set_opening","params":{{"id":{savings},"amount_minor":0}}}}"#),
        r#"{"id":8,"method":"db.check"}"#,
    ]);
    assert_eq!(out2[2]["error"]["code"], "bad_params");
    assert!(out2[2]["error"]["message"].as_str().unwrap().contains("counterweight"));
    assert!(err_of(&out2[3]).is_none(), "a normal account is fine: {}", out2[3]);
    assert_eq!(out2[5]["result"]["ok"], true, "the book still balances");
    assert!(out2[6]["result"]["removed"].is_i64(), "zero removes the transaction");
    assert_eq!(out2[7]["result"]["ok"], true);
}

/// Renaming is a label change and nothing else: the book refers to an account by id everywhere, so
/// every posting, count and balance must come through untouched. The UNIQUE constraint on
/// account.name is the interesting part -- it has to arrive as a refusal an operator can act on.
#[test]
fn an_account_can_be_renamed_but_never_onto_a_name_already_taken() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Currnet","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Rent","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"Rent",
             "postings":[{"account_id":1,"amount_minor":-90000},{"account_id":2,"amount_minor":90000}]}}"#,
        // Surrounding space is trimmed rather than stored: a name is a handle, not a layout.
        r#"{"id":4,"method":"account.rename","params":{"id":1,"name":" Current account "}}"#,
        // Saving a form without changing the field must not be an error.
        r#"{"id":5,"method":"account.rename","params":{"id":1,"name":"Current account"}}"#,
        r#"{"id":6,"method":"account.rename","params":{"id":2,"name":"Current account"}}"#,
        r#"{"id":7,"method":"account.rename","params":{"id":2,"name":" "}}"#,
        r#"{"id":8,"method":"account.rename","params":{"id":404,"name":"Ghost"}}"#,
        r#"{"id":9,"method":"account.list"}"#,
        r#"{"id":10,"method":"txn.get","params":{"id":1}}"#,
        r#"{"id":11,"method":"db.check"}"#,
    ]);

    assert_eq!(out[3]["result"]["name"], "Current account", "the name is stored trimmed");
    assert_eq!(out[3]["result"]["id"], 1);
    assert!(err_of(&out[4]).is_none(), "renaming to the current name is a no-op, not a clash");

    // The collision is reported as a collision, not as "UNIQUE constraint failed: account.name".
    assert_eq!(out[5]["error"]["code"], "already_exists");
    let msg = out[5]["error"]["message"].as_str().unwrap();
    assert!(msg.contains("Current account"), "the message names what is taken: {msg}");
    assert!(!msg.contains("UNIQUE"), "the raw constraint must not reach the operator: {msg}");

    // NOT NULL accepts a blank string happily; an account called " " is unfindable.
    assert_eq!(out[6]["error"]["code"], "bad_params");
    assert_eq!(out[7]["error"]["code"], "not_found");

    let listed = out[8]["result"].as_array().unwrap();
    let renamed = listed.iter().find(|a| a["name"] == "Current account").expect("renamed account");
    assert_eq!(renamed["id"], 1);
    assert_eq!(renamed["postings"], 1, "its history came with it -- references are by id");
    assert!(!listed.iter().any(|a| a["name"] == "Currnet"), "the typo is gone");
    assert!(listed.iter().any(|a| a["name"] == "Rent"), "the other account is untouched");

    // The transaction still reads the same, now under the corrected account name.
    let postings = out[9]["result"]["postings"].as_array().unwrap();
    let bank = postings.iter().find(|p| p["account_id"] == 1).unwrap();
    assert_eq!(bank["account"], "Current account");
    assert_eq!(bank["amount_minor"], -90_000, "renaming moved no money");

    assert_eq!(out[10]["result"]["ok"], true, "the book still balances");
}

/// The FX conversion accounts are the book's own machinery, not accounts anyone owns -- entry.rs
/// already refuses postings to them, and this is the same rule about the same accounts. (fx.rs no
/// longer depends on their names: roles.rs finds them by a `kind` the schema makes unforgeable. The
/// refusal is about ownership now, not about keeping a lookup working.)
#[test]
fn a_system_account_is_never_renamable() {
    let setup = [
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Euro pot","kind":"asset","currency":"EUR"}}"#,
        r#"{"id":3,"method":"txn.convert","params":{"occurred_on":"2026-08-01","description":"holiday money",
             "from_account":1,"from_minor":40000,"to_account":2,"to_minor":46664}}"#,
    ];
    let mut discover = setup.to_vec();
    discover.push(r#"{"id":4,"method":"account.list"}"#);
    let out = run(&discover);
    let sys: Vec<_> =
        out[3]["result"].as_array().unwrap().iter().filter(|a| a["system"] == true).collect();
    assert!(!sys.is_empty(), "a conversion should have created a system account");
    let id = sys[0]["id"].as_i64().unwrap();

    let attempt = format!(r#"{{"id":4,"method":"account.rename","params":{{"id":{id},"name":"My trading account"}}}}"#);
    let mut lines = setup.to_vec();
    lines.push(&attempt);
    lines.push(r#"{"id":5,"method":"account.list"}"#);
    let out = run(&lines);

    assert_eq!(out[3]["error"]["code"], "system_account", "{}", out[3]);
    let msg = out[3]["error"]["message"].as_str().unwrap();
    assert!(msg.contains("cannot be renamed"), "it must say what was refused: {msg}");
    let still: Vec<_> = out[4]["result"].as_array().unwrap().iter()
        .filter(|a| a["system"] == true).collect();
    assert!(still.iter().any(|a| a["name"].as_str().unwrap().starts_with("Conversion:")),
            "the machinery keeps the name fx.rs looks it up by");
}

/// The counterweight is an ORDINARY account -- not system -- so it can be renamed, and someone will.
///
/// account.set_opening used to resolve it BY NAME on its update path. A renamed counterweight was
/// therefore missed, a fresh equity account was silently created in its place, and the UPDATE against
/// that new account matched zero rows while the asset leg had already moved. The pair stopped summing
/// to zero, the handler still answered `updated: true`, and nothing in the UI runs db.check -- so the
/// book was left unbalanced with no surface anywhere that said so. Two clicks, in a panel whose own
/// help text promised no figure moves.
#[test]
fn renaming_the_counterweight_leaves_the_next_opening_edit_balanced() {
    let create = r#"{"id":1,"method":"account.create","params":{"name":"Bank","kind":"asset","currency":"GBP",
         "opening_minor":100000,"opening_on":"2026-01-01"}}"#;
    let out = run(&[create, r#"{"id":2,"method":"account.list"}"#]);
    let eq = out[1]["result"].as_array().unwrap().iter()
        .find(|a| a["name"] == "Opening balances (GBP)")
        .expect("the create wrote a counterweight").clone();
    let eq_id = eq["id"].as_i64().unwrap();
    assert_eq!(eq["system"], false, "it is deliberately renamable, which is why this test exists");

    let rename = format!(
        r#"{{"id":2,"method":"account.rename","params":{{"id":{eq_id},"name":"Seed money"}}}}"#);
    let out = run(&[
        create,
        &rename,
        // The edit that used to break it: change an opening balance the counterweight is party to.
        r#"{"id":3,"method":"account.set_opening","params":{"id":1,"amount_minor":123400}}"#,
        r#"{"id":4,"method":"db.check"}"#,
        r#"{"id":5,"method":"account.balances"}"#,
        r#"{"id":6,"method":"account.opening","params":{"id":1}}"#,
        // A SECOND opening balance must reuse the renamed counterweight, not grow another one.
        r#"{"id":7,"method":"account.create","params":{"name":"Savings","kind":"asset","currency":"GBP",
             "opening_minor":50000,"opening_on":"2026-01-01"}}"#,
        r#"{"id":8,"method":"account.list"}"#,
        r#"{"id":9,"method":"db.check"}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }
    assert_eq!(out[3]["result"]["ok"], true, "the edit must not unbalance the book: {}", out[3]);

    let bal = |i: usize, name: &str| -> i64 {
        out[i]["result"].as_array().unwrap().iter()
            .find(|b| b["name"] == name).unwrap_or_else(|| panic!("no {name}"))["balance_minor"]
            .as_i64().unwrap()
    };
    assert_eq!(bal(4, "Bank"), 123_400);
    assert_eq!(bal(4, "Seed money"), -123_400, "the counterweight absorbed the whole change");

    // Still found structurally, by the equity KIND of its far leg rather than by any name.
    assert_eq!(out[5]["result"]["amount_minor"], 123_400);

    let equities: Vec<_> = out[7]["result"].as_array().unwrap().iter()
        .filter(|a| a["kind"] == "equity").collect();
    assert_eq!(equities.len(), 1, "a renamed counterweight must be reused, not duplicated: {equities:?}");
    assert_eq!(equities[0]["name"], "Seed money");
    assert_eq!(out[8]["result"]["ok"], true);
}

/// The system guard was complete in ONE direction only: it stopped a magic account being renamed
/// AWAY from its name, and nothing stopped an ordinary account being renamed INTO one. The collision
/// check only refuses names already taken, so any reserved name the book did not yet hold was free to
/// squat -- and every squat corrupted silently, with db.check green throughout.
///
/// roles.rs means a squat can no longer redirect anything. This is the second half: the core keeps
/// the names it creates its own accounts with, so a squat cannot collide with them later either.
#[test]
fn an_ordinary_account_cannot_squat_on_a_name_the_core_owns() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Savings","kind":"asset","currency":"GBP"}}"#,
        // A: the compounding one -- set_opening would have posted counterweights into a real asset.
        r#"{"id":2,"method":"account.rename","params":{"id":1,"name":"Opening balances (GBP)"}}"#,
        // B: a real EUR account taking the conversion leg and sitting permanently negative.
        r#"{"id":3,"method":"account.rename","params":{"id":1,"name":"Conversion:EUR"}}"#,
        // C: the importer's bucket, which analysis.rs sums as uncategorised spend.
        r#"{"id":4,"method":"account.rename","params":{"id":1,"name":"Expenses:Unclassified"}}"#,
        // The same hole at creation: a reserved name for a currency the book has never held.
        r#"{"id":5,"method":"account.create","params":{"name":"Conversion:USD","kind":"asset","currency":"USD"}}"#,
        r#"{"id":6,"method":"account.list"}"#,
    ]);
    for (i, r) in out.iter().enumerate().skip(1).take(4) {
        assert_eq!(r["error"]["code"], "reserved_name", "reply {i}: {r}");
        let msg = r["error"]["message"].as_str().unwrap();
        assert!(msg.contains("the book keeps that name for it"), "{msg}");
    }
    let listed = out[5]["result"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "nothing was created or renamed: {listed:?}");
    assert_eq!(listed[0]["name"], "Savings");
}

/// The counterweight and the importer's bucket are ordinary accounts wearing a reserved name, and a
/// form that saves an unchanged field must not be an error. The refusal is about MOVING a name onto
/// an account, so the account that already holds it is unaffected.
#[test]
fn an_account_already_holding_a_reserved_name_can_still_save_it_unchanged() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Bank","kind":"asset","currency":"GBP",
             "opening_minor":100000,"opening_on":"2026-01-01"}}"#,
        r#"{"id":2,"method":"account.rename","params":{"id":2,"name":"Opening balances (GBP)"}}"#,
        r#"{"id":3,"method":"db.check"}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "{r}");
    }
    assert_eq!(out[1]["result"]["name"], "Opening balances (GBP)");
    assert_eq!(out[2]["result"]["ok"], true);
}

/// A description is a label and labels get edited -- "CARD PAYMENT 4412" means nothing six months
/// later. What it must NOT do is disturb the record: the date, the postings and the amounts are
/// what actually happened.
#[test]
fn a_payment_can_be_relabelled_without_touching_what_it_records() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Groceries","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"CARD PAYMENT 4412",
             "postings":[{"account_id":1,"amount_minor":-4599},{"account_id":2,"amount_minor":4599}]}}"#,
        r#"{"id":4,"method":"txn.get","params":{"id":1}}"#,
        r#"{"id":5,"method":"txn.rename","params":{"id":1,"description":" Weekly shop "}}"#,
        r#"{"id":6,"method":"txn.get","params":{"id":1}}"#,
        r#"{"id":7,"method":"txn.rename","params":{"id":1,"description":" "}}"#,
        r#"{"id":8,"method":"txn.rename","params":{"id":404,"description":"Ghost"}}"#,
        r#"{"id":9,"method":"txn.rename","params":{"id":1}}"#,
        r#"{"id":10,"method":"db.check"}"#,
    ]);

    let before = out[3]["result"].clone();
    assert_eq!(out[4]["result"]["description"], "Weekly shop", "stored trimmed");
    assert_eq!(out[4]["result"]["id"], 1);

    let after = out[5]["result"].clone();
    assert_eq!(after["description"], "Weekly shop");
    assert_eq!(after["occurred_on"], before["occurred_on"], "the date is not a label");
    assert_eq!(after["postings"], before["postings"], "no money moved and no posting id changed");

    assert_eq!(out[6]["error"]["code"], "bad_params", "a blank description is refused");
    assert_eq!(out[7]["error"]["code"], "not_found");
    assert_eq!(out[8]["error"]["code"], "bad_params", "a missing description is refused");
    assert_eq!(out[9]["result"]["ok"], true);
}

/// A wrong amount, a wrong day or a wrong account is a mistake in the record, and the fix must not
/// cost what hangs off the payment's id -- here, the link that threads it to the refund. Deleting
/// and retyping would drop that; txn.update keeps it, and the browser sees the corrected payment
/// under the same id.
#[test]
fn a_payment_can_be_corrected_without_losing_what_hangs_off_it() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Shop","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Groceries","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":4,"method":"account.create","params":{"name":"Euro pot","kind":"asset","currency":"EUR"}}"#,
        r#"{"id":5,"method":"txn.create","params":{"occurred_on":"2026-08-25","description":"TESCO run",
             "payee":"Tesco","postings":[{"account_id":1,"amount_minor":-1000},{"account_id":2,"amount_minor":1000}]}}"#,
        r#"{"id":6,"method":"txn.create","params":{"occurred_on":"2026-08-26","description":"refund",
             "postings":[{"account_id":1,"amount_minor":500},{"account_id":2,"amount_minor":-500}]}}"#,
        r#"{"id":7,"method":"link.create","params":{"from_txn":1,"to_txn":2}}"#,
        // It was £12.50 on the 24th, and it belongs in Groceries.
        r#"{"id":8,"method":"txn.update","params":{"id":1,"occurred_on":"2026-08-24","description":" Weekly shop ",
             "postings":[{"account_id":1,"amount_minor":-1250},{"account_id":3,"amount_minor":1250}]}}"#,
        r#"{"id":9,"method":"link.chain","params":{"txn_id":2}}"#,
        r#"{"id":10,"method":"txn.browse","params":{"account_id":3}}"#,
        r#"{"id":11,"method":"account.balances"}"#,
        r#"{"id":12,"method":"txn.update","params":{"id":1,"description":"  "}}"#,
        r#"{"id":13,"method":"txn.update","params":{"id":1,"occurred_on":"2026-13-01"}}"#,
        r#"{"id":14,"method":"txn.update","params":{"id":404,"description":"Ghost"}}"#,
        r#"{"id":15,"method":"txn.update","params":{"id":1,"postings":[{"account_id":1,"amount_minor":-1250},{"account_id":3,"amount_minor":1200}]}}"#,
        r#"{"id":16,"method":"txn.update","params":{"id":1}}"#,
        // The same payment turns out to have been money sent to the euro pot: a conversion.
        r#"{"id":17,"method":"txn.update","params":{"id":1,"conversion":{"from_account":1,"from_minor":1250,"to_account":4,"to_minor":1450}}}"#,
        r#"{"id":18,"method":"txn.get","params":{"id":1}}"#,
        r#"{"id":19,"method":"db.check"}"#,
    ]);

    let edited = &out[7]["result"];
    assert!(err_of(&out[7]).is_none(), "{}", out[7]);
    assert_eq!(edited["id"], 1, "same payment, corrected");
    assert_eq!(edited["occurred_on"], "2026-08-24");
    assert_eq!(edited["description"], "Weekly shop", "stored trimmed");
    assert_eq!(edited["payee"], "Tesco", "not mentioned, so not touched");
    let legs = edited["postings"].as_array().unwrap();
    assert_eq!(legs.len(), 2);
    assert!(legs.iter().any(|p| p["account"] == "Groceries" && p["amount_minor"] == 1250));

    // The thread survived the edit, and reads the corrected payment.
    let chain = &out[8]["result"];
    assert_eq!(chain["nodes"].as_array().unwrap().len(), 2, "still linked to the refund");
    let node = chain["nodes"].as_array().unwrap().iter().find(|n| n["id"] == 1).unwrap();
    assert_eq!(node["description"], "Weekly shop");
    assert_eq!(node["occurred_on"], "2026-08-24");

    // The browser finds it under its new account, and the balances moved with it.
    assert_eq!(out[9]["result"]["total"], 1);
    let bal = |name: &str| -> i64 {
        out[10]["result"].as_array().unwrap().iter().find(|b| b["name"] == name).unwrap()
            ["balance_minor"].as_i64().unwrap()
    };
    assert_eq!(bal("Groceries"), 1250);
    assert_eq!(bal("Shop"), -500, "only the refund is left there");
    assert_eq!(bal("Current"), -750);

    assert_eq!(out[11]["error"]["code"], "bad_params", "a blank description is refused");
    assert_eq!(out[12]["error"]["code"], "bad_params", "a day that does not exist is refused");
    assert!(out[12]["error"]["message"].as_str().unwrap().contains("2026-13-01"), "{}", out[12]);
    assert_eq!(out[13]["error"]["code"], "not_found");
    assert_eq!(out[14]["error"]["code"], "unbalanced", "the same rule as recording it");
    assert!(err_of(&out[15]).is_none(), "saving a form untouched changes nothing and is not an error");
    assert_eq!(out[15]["result"]["occurred_on"], "2026-08-24", "the refusals wrote nothing");

    assert!(err_of(&out[16]).is_none(), "{}", out[16]);
    let after = &out[17]["result"];
    assert_eq!(after["postings"].as_array().unwrap().len(), 4, "two real legs, two conversion legs");
    assert!(after["postings"].as_array().unwrap().iter().any(|p| p["account"] == "Euro pot" && p["amount_minor"] == 1450));
    assert_eq!(after["description"], "Weekly shop", "the legs changed, the words did not");
    assert_eq!(out[18]["result"]["ok"], true, "the book balances after every edit");
}

/// The graph view's model: an unlinked payment is two visits and an arrow, a link between
/// payments that touch the same account fuses that visit, and income and expense accounts are one
/// shared node each. The shape worth pinning down is two chains converging: both arrive at the
/// same friend, one payment leaves, and that visit must come back as ONE node with two in and one
/// out rather than as three visits that happen to share a name.
#[test]
fn the_graph_fuses_linked_visits_and_shares_the_ends() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Salary","kind":"income","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Sam","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":4,"method":"account.create","params":{"name":"Bookmaker","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":5,"method":"account.create","params":{"name":"Wallet","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":6,"method":"account.create","params":{"name":"Groceries","kind":"expense","currency":"GBP"}}"#,
        // chain one: salary in, then to Sam, then Sam to the bookmaker
        r#"{"id":7,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"pay",
             "postings":[{"account_id":1,"amount_minor":-100000},{"account_id":2,"amount_minor":100000}]}}"#,
        r#"{"id":8,"method":"txn.create","params":{"occurred_on":"2026-08-02","description":"to Sam",
             "postings":[{"account_id":2,"amount_minor":-5000},{"account_id":3,"amount_minor":5000}]}}"#,
        r#"{"id":9,"method":"txn.create","params":{"occurred_on":"2026-08-03","description":"Sam bets",
             "postings":[{"account_id":3,"amount_minor":-8000},{"account_id":4,"amount_minor":8000}]}}"#,
        // chain two converges on the same visit to Sam
        r#"{"id":10,"method":"txn.create","params":{"occurred_on":"2026-08-02","description":"cash to Sam",
             "postings":[{"account_id":5,"amount_minor":-3000},{"account_id":3,"amount_minor":3000}]}}"#,
        // two payments nobody linked: a shop, and another salary
        r#"{"id":11,"method":"txn.create","params":{"occurred_on":"2026-08-04","description":"shop",
             "postings":[{"account_id":2,"amount_minor":-2000},{"account_id":6,"amount_minor":2000}]}}"#,
        r#"{"id":12,"method":"txn.create","params":{"occurred_on":"2026-09-01","description":"pay",
             "postings":[{"account_id":1,"amount_minor":-100000},{"account_id":2,"amount_minor":100000}]}}"#,
        r#"{"id":13,"method":"link.create","params":{"from_txn":1,"to_txn":2}}"#,
        r#"{"id":14,"method":"link.create","params":{"from_txn":2,"to_txn":3}}"#,
        r#"{"id":15,"method":"link.create","params":{"from_txn":4,"to_txn":3}}"#,
        // An opening balance is a starting position, not a payment: it must not be in the picture.
        r#"{"id":98,"method":"account.set_opening","params":{"id":5,"amount_minor":50000,"occurred_on":"2026-07-01"}}"#,
        r#"{"id":16,"method":"link.graph","params":{}}"#,
        // a link between payments with no account in common is kept, but as an assertion
        r#"{"id":17,"method":"link.create","params":{"from_txn":5,"to_txn":4}}"#,
        r#"{"id":18,"method":"link.graph","params":{"from":"2026-08-01","to":"2026-08-31"}}"#,
    ]);
    assert!(err_of(&out[15]).is_none(), "{}", out[15]);
    let g = &out[16]["result"];
    assert!(err_of(&out[16]).is_none(), "{}", out[16]);
    let nodes = g["nodes"].as_array().unwrap();
    let edges = g["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 6, "one arrow per payment, and none for the opening balance");
    assert_eq!(g["payments"], 6, "the opening balance is not counted either");
    assert_eq!(g["total"], 6);
    assert!(!nodes.iter().any(|n| n["kind"] == "equity"), "no counterweight node: {nodes:?}");

    // The two salaries diverge from ONE Salary node; the shop merges into ONE Groceries node.
    let salary: Vec<_> = nodes.iter().filter(|n| n["account"] == "Salary").collect();
    assert_eq!(salary.len(), 1);
    assert_eq!(salary[0]["shared"], true);
    assert_eq!(salary[0]["out"], 2);
    assert_eq!(nodes.iter().filter(|n| n["account"] == "Groceries").count(), 1);

    // Sam was visited once by three payments: the fused node carries all of them.
    let sam: Vec<_> = nodes.iter().filter(|n| n["account"] == "Sam").collect();
    assert_eq!(sam.len(), 1, "converging chains share the visit");
    assert_eq!(sam[0]["txn_ids"], serde_json::json!([2, 3, 4]));
    assert_eq!(sam[0]["in"], 2);
    assert_eq!(sam[0]["out"], 1);
    assert_eq!(sam[0]["shared"], false);

    // Current: one visit for the linked salary-then-to-Sam pair, and one each for the unlinked
    // shop and the unlinked second salary.
    let current: Vec<_> = nodes.iter().filter(|n| n["account"] == "Current").collect();
    assert_eq!(current.len(), 3, "unlinked payments do not share a visit");
    assert!(current.iter().any(|n| n["txn_ids"] == serde_json::json!([1, 2])));
    assert!(current.iter().any(|n| n["txn_ids"] == serde_json::json!([5])));

    // Arrows run from the leaving leg to the arriving one and carry the amount.
    let bet = edges.iter().find(|e| e["txn_id"] == 3).unwrap();
    assert_eq!(bet["from"], sam[0]["id"]);
    assert_eq!(bet["amount_minor"], 8000);
    let bookie = &nodes[bet["to"].as_u64().unwrap() as usize];
    assert_eq!(bookie["account"], "Bookmaker");

    let links = g["links"].as_array().unwrap();
    assert_eq!(links.len(), 3);
    assert!(links.iter().all(|l| l["shared"] == true), "every link here is a fused visit");

    // The window drops September's salary, and the loose link is drawn between visits rather
    // than fusing anything: the shop and the cash to Sam touch no account in common.
    let g = &out[18]["result"];
    assert_eq!(g["payments"], 5);
    let loose: Vec<_> = g["links"].as_array().unwrap().iter().filter(|l| l["shared"] == false).collect();
    assert_eq!(loose.len(), 1);
    assert_eq!(loose[0]["from_txn"], 5);
    let nodes = g["nodes"].as_array().unwrap();
    assert_eq!(nodes[loose[0]["from_node"].as_u64().unwrap() as usize]["account"], "Groceries");
    assert_eq!(nodes[loose[0]["to_node"].as_u64().unwrap() as usize]["account"], "Wallet");
    assert_eq!(nodes.iter().filter(|n| n["account"] == "Sam").count(), 1, "still one visit to Sam");
}

/// The forecast has always skipped a slot a real transaction claims, but nothing ever claimed
/// one, so a paid rent stayed upcoming and was counted twice. series.record is the missing half:
/// it writes the payment from the template (or the amount that really moved), claims the slot,
/// and the next projection shows the real payment on its day instead of the projection.
#[test]
fn recording_an_occurrence_retires_it_from_the_forecast() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Rent","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"series.create","params":{"description":"Rent","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-01-01","postings":[{"account_id":1,"amount_minor":-90000,"role":"primary"},
                                                {"account_id":2,"amount_minor":90000,"role":"balancing"}]}}"#,
        r#"{"id":4,"method":"forecast.project","params":{"as_of":"2026-09-05","horizon":"2026-11-30"}}"#,
        // October's rent went out on the 2nd, and was £950 this time.
        r#"{"id":5,"method":"series.record","params":{"series_id":1,"occurrence_on":"2026-10-01",
             "occurred_on":"2026-10-02","amount_minor":95000}}"#,
        r#"{"id":6,"method":"txn.get","params":{"id":1}}"#,
        r#"{"id":7,"method":"forecast.project","params":{"as_of":"2026-09-05","horizon":"2026-11-30"}}"#,
        r#"{"id":8,"method":"series.record","params":{"series_id":1,"occurrence_on":"2026-10-01"}}"#,
        r#"{"id":9,"method":"series.override","params":{"series_id":1,"occurrence_on":"2026-11-01","action":"skip"}}"#,
        r#"{"id":10,"method":"series.record","params":{"series_id":1,"occurrence_on":"2026-11-01"}}"#,
        r#"{"id":11,"method":"series.record","params":{"series_id":404,"occurrence_on":"2026-11-01"}}"#,
        // Editing the recorded payment keeps its claim: the date moves, the slot does not.
        r#"{"id":12,"method":"txn.update","params":{"id":1,"occurred_on":"2026-10-03"}}"#,
        r#"{"id":13,"method":"forecast.project","params":{"as_of":"2026-09-05","horizon":"2026-11-30"}}"#,
        r#"{"id":14,"method":"account.balances","params":{"as_of":"2026-09-05"}}"#,
        r#"{"id":15,"method":"db.check"}"#,
    ]);
    let before = out[3]["result"]["occurrences"].as_array().unwrap();
    assert!(before.iter().any(|o| o["occurrence_on"] == "2026-10-01" && o["series_id"] == 1));
    assert!(before.iter().all(|o| o["txn_id"].is_null()), "nothing real yet");

    assert!(err_of(&out[4]).is_none(), "{}", out[4]);
    assert_eq!(out[4]["result"]["txn_ids"], serde_json::json!([1]));
    let t = &out[5]["result"];
    assert_eq!(t["occurred_on"], "2026-10-02");
    assert_eq!(t["description"], "Rent");
    let legs = t["postings"].as_array().unwrap();
    assert!(legs.iter().any(|p| p["account"] == "Current" && p["amount_minor"] == -95_000));
    assert!(legs.iter().any(|p| p["account"] == "Rent" && p["amount_minor"] == 95_000));

    let after = out[6]["result"]["occurrences"].as_array().unwrap();
    assert!(!after.iter().any(|o| o["occurrence_on"] == "2026-10-01" && o["kind"] == "series"),
            "the slot is no longer projected");
    let real: Vec<_> = after.iter().filter(|o| o["txn_id"] == 1).collect();
    assert_eq!(real.len(), 2, "the real payment is listed instead, both legs");
    assert_eq!(real[0]["kind"], "real");
    assert_eq!(real[0]["value_on"], "2026-10-02");
    assert_eq!(real[0]["series_id"], 1, "still attributed to its series");
    assert_eq!(real[0]["occurrence_on"], "2026-10-01", "and to its slot");
    assert!(after.iter().any(|o| o["occurrence_on"] == "2026-11-01" && o["series_id"] == 1),
            "November is still to come");

    assert_eq!(out[7]["error"]["code"], "already_recorded");
    assert_eq!(out[9]["error"]["code"], "bad_params", "a skipped slot is not recorded blindly");
    assert!(out[9]["error"]["message"].as_str().unwrap().contains("skipped"));
    assert_eq!(out[10]["error"]["code"], "bad_params");

    assert!(err_of(&out[11]).is_none(), "{}", out[11]);
    let moved = out[12]["result"]["occurrences"].as_array().unwrap();
    assert!(!moved.iter().any(|o| o["occurrence_on"] == "2026-10-01" && o["kind"] == "series"),
            "still claimed after the edit");
    assert!(moved.iter().any(|o| o["txn_id"] == 1 && o["value_on"] == "2026-10-03"));

    // The balance as of today does not include October's rent; the forecast does, on its day.
    let cur = out[13]["result"].as_array().unwrap().iter().find(|b| b["name"] == "Current").unwrap();
    assert_eq!(cur["balance_minor"], 0);
    assert_eq!(out[14]["result"]["ok"], true);
}

/// A recurring chain is recorded as a chain: every hop of the wave, linked hop to hop, each slot
/// claimed, all or none.
#[test]
fn recording_a_chain_occurrence_writes_and_links_every_hop() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Sam","kind":"asset"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Bookmaker","kind":"asset"}}"#,
        r#"{"id":4,"method":"series.create_chain","params":{"description":"stake via Sam",
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=1","dtstart":"2026-10-01","from_account":1,
             "hops":[{"to_account":2,"amount_minor":10000},{"to_account":3,"amount_minor":9500}]}}"#,
        // £110 went out instead of £100: the second hop follows as a difference, so the £5 fee
        // taken by Sam is still £5.
        r#"{"id":5,"method":"series.record","params":{"series_id":1,"occurrence_on":"2026-10-01",
             "whole_chain":true,"amount_minor":11000}}"#,
        r#"{"id":6,"method":"link.chain","params":{"txn_id":1}}"#,
        r#"{"id":7,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-11-30"}}"#,
        r#"{"id":8,"method":"series.record","params":{"series_id":2,"occurrence_on":"2026-10-01","whole_chain":true}}"#,
        r#"{"id":9,"method":"db.check"}"#,
    ]);
    assert!(err_of(&out[4]).is_none(), "{}", out[4]);
    assert_eq!(out[4]["result"]["txn_ids"], serde_json::json!([1, 2]));
    let chain = &out[5]["result"];
    assert_eq!(chain["nodes"].as_array().unwrap().len(), 2, "linked hop to hop");
    let amount_of = |id: i64| -> i64 {
        chain["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id).unwrap()["postings"]
            .as_array().unwrap().iter().map(|p| p["amount_minor"].as_i64().unwrap().abs()).max().unwrap()
    };
    assert_eq!(amount_of(1), 11_000, "the typed amount on the hop it was typed for");
    assert_eq!(amount_of(2), 10_500, "the next hop moved by the same difference");
    let proj = out[6]["result"]["occurrences"].as_array().unwrap();
    assert!(!proj.iter().any(|o| o["occurrence_on"] == "2026-10-01" && o["kind"] == "series"),
            "October's wave is claimed for every hop");
    assert!(proj.iter().any(|o| o["occurrence_on"] == "2026-11-01" && o["series_id"] == 1), "November remains");
    assert_eq!(proj.iter().filter(|o| !o["txn_id"].is_null()).count(), 4, "two real payments, two legs each");
    assert_eq!(out[7]["error"]["code"], "already_recorded", "the second hop's slot is taken too");
    assert_eq!(out[8]["result"]["ok"], true);
}

/// A what-if is a question, not a payment: its occurrences are drawn, but recording one would put
/// hypothetical money in the book and pin the scenario there for good. And the brief must keep
/// counting a commitment after one of its slots has been recorded: the payment is still rent.
#[test]
fn a_what_if_cannot_be_recorded_and_a_recorded_slot_still_counts_as_a_commitment() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Rent","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"scenario.create","params":{"name":"bigger flat"}}"#,
        r#"{"id":4,"method":"series.create","params":{"description":"Dream rent","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","scenario_id":1,"postings":[{"account_id":1,"amount_minor":-150000,"role":"primary"},
                                                {"account_id":2,"amount_minor":150000,"role":"balancing"}]}}"#,
        r#"{"id":5,"method":"series.record","params":{"series_id":1,"occurrence_on":"2026-10-01"}}"#,
        r#"{"id":6,"method":"forecast.project","params":{"as_of":"2026-09-05","horizon":"2026-10-31","scenarios":[1]}}"#,
        r#"{"id":7,"method":"series.create","params":{"description":"Rent","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-01-01","postings":[{"account_id":1,"amount_minor":-90000,"role":"primary"},
                                                {"account_id":2,"amount_minor":90000,"role":"balancing"}]}}"#,
        r#"{"id":8,"method":"analysis.brief","params":{"as_of":"2026-09-05","months":3}}"#,
        r#"{"id":9,"method":"series.record","params":{"series_id":2,"occurrence_on":"2026-10-01"}}"#,
        r#"{"id":10,"method":"analysis.brief","params":{"as_of":"2026-09-05","months":3}}"#,
        r#"{"id":11,"method":"txn.list"}"#,
    ]);
    assert_eq!(out[4]["error"]["code"], "bad_params", "{}", out[4]);
    assert!(out[4]["error"]["message"].as_str().unwrap().contains("what-if"));
    let occ = out[5]["result"]["occurrences"].as_array().unwrap();
    let dream = occ.iter().find(|o| o["series_id"] == 1).expect("the what-if is still projected");
    assert_eq!(dream["scenario_id"], 1, "and says which scenario it belongs to");

    let rent_line = |brief: &serde_json::Value| -> serde_json::Value {
        brief["commitments"]["series"].as_array().unwrap().iter()
            .find(|c| c["description"] == "Rent").cloned().unwrap()
    };
    let before = rent_line(&out[7]["result"]);
    let after = rent_line(&out[9]["result"]);
    assert_eq!(before["occurrences_next_12m"], 12);
    assert_eq!(after["occurrences_next_12m"], 12, "recording October's rent does not make it not rent");
    assert_eq!(after["annual_minor"], before["annual_minor"]);
    assert_eq!(after["next_on"], "2026-10-01", "the recorded payment is the next one");

    // Today's position does not count October's rent; it is in the outlook's dip instead. The
    // only posting on Current is that one, so as of today the account has no position at all.
    let position = out[9]["result"]["position"]["by_account"].as_array().unwrap();
    assert!(position.iter().all(|a| a["balance_minor"] == 0), "{position:?}");
    assert_eq!(out[10]["result"].as_array().unwrap().len(), 1, "one real payment written");
}

/// The one that matters: renaming a PLAN must not rewrite HISTORY.
///
/// A projected occurrence takes its description from the series row, so a transaction generated
/// from a series carries a copy of the description the series had at the time. That copy is the
/// record of what happened and stays as written -- rewriting it would silently alter entries the
/// operator has already reconciled against a statement. Everything still to come picks the new
/// description up on the next projection, which is the whole of what a rename should do.
#[test]
fn renaming_a_plan_leaves_the_payments_it_already_made_alone() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Bank","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Gym","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"series.create","params":{"description":"Gym","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-01-01","postings":[{"account_id":1,"amount_minor":-3000,"role":"primary"},
                                                {"account_id":2,"amount_minor":3000,"role":"balancing"}]}}"#,
        // An occurrence that has already happened, written with the description the series had.
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"Gym",
             "postings":[{"account_id":1,"amount_minor":-3000},{"account_id":2,"amount_minor":3000}]}}"#,
        r#"{"id":5,"method":"series.rename","params":{"id":1,"description":" Gym membership "}}"#,
        r#"{"id":6,"method":"series.list"}"#,
        r#"{"id":7,"method":"txn.get","params":{"id":1}}"#,
        r#"{"id":8,"method":"forecast.project","params":{"as_of":"2026-08-31","horizon":"2026-12-31"}}"#,
        r#"{"id":9,"method":"series.rename","params":{"id":1,"description":" "}}"#,
        r#"{"id":10,"method":"series.rename","params":{"id":404,"description":"Ghost"}}"#,
        r#"{"id":11,"method":"db.check"}"#,
    ]);

    assert_eq!(out[4]["result"]["description"], "Gym membership", "stored trimmed");
    assert_eq!(out[4]["result"]["id"], 1);
    assert_eq!(out[5]["result"][0]["description"], "Gym membership", "the plan reads as renamed");

    // The whole point.
    assert_eq!(out[6]["result"]["description"], "Gym",
               "a payment already made is a historical record, not a copy of the plan");

    // ...while everything still ahead carries the new name.
    let occ = out[7]["result"]["occurrences"].as_array().unwrap();
    let future: Vec<&serde_json::Value> = occ.iter().filter(|o| o["series_id"] == 1).collect();
    assert!(!future.is_empty(), "the series still projects after being renamed");
    assert!(future.iter().all(|o| o["description"] == "Gym membership"),
            "the forecast reads the series row live: {:?}", future.first());

    assert_eq!(out[8]["error"]["code"], "bad_params", "a blank description is refused");
    assert_eq!(out[9]["error"]["code"], "not_found");
    assert_eq!(out[10]["result"]["ok"], true);
}

/// The description box is a shortcut, not a vocabulary. What matters is that the labels she types
/// every month float to the top of it -- ordering by recency instead would put yesterday's one-off
/// above the weekly shop, which is exactly backwards for something meant to save typing.
#[test]
fn descriptions_offer_the_labels_she_types_most_and_filter_case_insensitively() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Shop","kind":"expense","currency":"GBP"}}"#,
        // Three shops, but all of them OLD -- the frequent label is also the least recent one, so
        // count and recency disagree and the assertion below can only pass one way round.
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-01-05","description":"Weekly shop",
             "postings":[{"account_id":1,"amount_minor":-1000},{"account_id":2,"amount_minor":1000}]}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-02-05","description":"Weekly shop",
             "postings":[{"account_id":1,"amount_minor":-1000},{"account_id":2,"amount_minor":1000}]}}"#,
        r#"{"id":5,"method":"txn.create","params":{"occurred_on":"2026-03-05","description":"Weekly shop",
             "postings":[{"account_id":1,"amount_minor":-1000},{"account_id":2,"amount_minor":1000}]}}"#,
        // Rent and Coffee are used equally often, and only last_on separates them.
        r#"{"id":6,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"Rent",
             "postings":[{"account_id":1,"amount_minor":-5000},{"account_id":2,"amount_minor":5000}]}}"#,
        r#"{"id":7,"method":"txn.create","params":{"occurred_on":"2026-09-01","description":"Rent",
             "postings":[{"account_id":1,"amount_minor":-5000},{"account_id":2,"amount_minor":5000}]}}"#,
        r#"{"id":8,"method":"txn.create","params":{"occurred_on":"2026-04-01","description":"Coffee",
             "postings":[{"account_id":1,"amount_minor":-300},{"account_id":2,"amount_minor":300}]}}"#,
        r#"{"id":9,"method":"txn.create","params":{"occurred_on":"2026-05-01","description":"Coffee",
             "postings":[{"account_id":1,"amount_minor":-300},{"account_id":2,"amount_minor":300}]}}"#,
        r#"{"id":10,"method":"txn.create","params":{"occurred_on":"2026-09-02","description":"One-off gift",
             "postings":[{"account_id":1,"amount_minor":-2000},{"account_id":2,"amount_minor":2000}]}}"#,
        r#"{"id":11,"method":"txn.create","params":{"occurred_on":"2026-09-03","description":"50% bonus",
             "postings":[{"account_id":1,"amount_minor":9000},{"account_id":2,"amount_minor":-9000}]}}"#,
        r#"{"id":12,"method":"txn.descriptions"}"#,
        r#"{"id":13,"method":"txn.descriptions","params":{"prefix":"SHOP"}}"#,
        r#"{"id":14,"method":"txn.descriptions","params":{"prefix":"%"}}"#,
        r#"{"id":15,"method":"txn.descriptions","params":{"limit":2}}"#,
        r#"{"id":16,"method":"txn.descriptions","params":{"prefix":" "}}"#,
        r#"{"id":17,"method":"db.check"}"#,
    ]);

    let all = out[11]["result"].as_array().expect("a plain array of rows");
    let order: Vec<&str> = all.iter().map(|r| r["description"].as_str().unwrap()).collect();
    assert_eq!(
        order,
        vec!["Weekly shop", "Rent", "Coffee", "50% bonus", "One-off gift"],
        "commonest first, then most recently used; NOT newest first"
    );
    assert_eq!(all[0]["count"], 3);
    assert_eq!(all[0]["last_on"], "2026-03-05", "the count winner is the oldest label of the lot");
    // Rent and Coffee are both used twice, so the only thing that can have separated them is the
    // date of the last one.
    assert_eq!(all[1]["count"], 2);
    assert_eq!(all[1]["last_on"], "2026-09-01");
    assert_eq!(all[2]["count"], 2);
    assert_eq!(all[2]["last_on"], "2026-05-01");

    // Case-insensitive, and matching ANYWHERE: "shop" is the second word of "Weekly shop", so a
    // strict prefix match would return nothing here.
    let shop = out[12]["result"].as_array().unwrap();
    assert_eq!(shop.len(), 1, "{shop:?}");
    assert_eq!(shop[0]["description"], "Weekly shop");

    // A bare "%" is a LIKE wildcard. Unescaped it would match every description in the book, and
    // the box would look like it was ignoring what she typed.
    let pct = out[13]["result"].as_array().unwrap();
    assert_eq!(pct.len(), 1, "the wildcard is escaped, not honoured: {pct:?}");
    assert_eq!(pct[0]["description"], "50% bonus");

    let two = out[14]["result"].as_array().unwrap();
    assert_eq!(two.len(), 2, "limit is honoured");
    assert_eq!(two[0]["description"], "Weekly shop", "and it truncates the ranked list, not a random one");
    assert_eq!(two[1]["description"], "Rent");

    // A box that has been typed into and cleared again sends whitespace, which is not a filter.
    assert_eq!(out[15]["result"].as_array().unwrap().len(), 5, "a blank prefix filters nothing");

    assert_eq!(out[16]["result"]["ok"], true);
}

/// A label typed exactly as the book holds it has to find itself, whatever alphabet it is in.
///
/// It did not: the pattern was folded in Rust (Unicode-aware) and the column in SQL (ASCII-only),
/// so any uppercase non-ASCII letter landed on a different case on the two sides and the row was
/// unreachable -- "CAFÉ NERO" could be found by "CAF" and by "NERO" but never by "CAFÉ". Imported
/// bank descriptions are routinely all-capitals, so this was the everyday path, and the failure
/// looked to the operator like the suggestion box being broken.
#[test]
fn descriptions_find_an_accented_label_typed_exactly_as_it_is_stored() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Coffee","kind":"expense","currency":"GBP"}}"#,
        r#"{"id":3,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"CAFÉ NERO",
             "postings":[{"account_id":1,"amount_minor":-350},{"account_id":2,"amount_minor":350}]}}"#,
        r#"{"id":4,"method":"txn.descriptions","params":{"prefix":"CAFÉ NERO"}}"#,
        r#"{"id":5,"method":"txn.descriptions","params":{"prefix":"CAFÉ"}}"#,
        r#"{"id":6,"method":"txn.descriptions","params":{"prefix":"café"}}"#,
        r#"{"id":7,"method":"txn.descriptions","params":{"prefix":"NERO"}}"#,
        r#"{"id":8,"method":"txn.descriptions","params":{"prefix":"nero"}}"#,
    ]);

    for (i, typed) in [(3, "CAFÉ NERO"), (4, "CAFÉ"), (6, "NERO"), (7, "nero")] {
        let rows = out[i]["result"].as_array().unwrap_or_else(|| panic!("{}", out[i]));
        assert_eq!(rows.len(), 1, "typing {typed:?} finds the label it names: {rows:?}");
        assert_eq!(rows[0]["description"], "CAFÉ NERO");
    }
    // ASCII case-insensitivity is what LIKE gives; the É keeps its case on both sides, so a
    // lowercase "café" is a different string and misses. That is the honest limit of the fold, and
    // it is not the case that matters: nobody types an accent they cannot see in the book.
    assert_eq!(out[5]["result"].as_array().unwrap().len(), 0, "ASCII fold only, symmetrically");
}

/// A book with no payments in it yet is the FIRST thing the form meets, so the suggestion list has
/// to come back empty rather than as an error -- an error here would break the panel on a new book.
#[test]
fn an_empty_book_offers_an_empty_description_list() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset","currency":"GBP"}}"#,
        r#"{"id":2,"method":"txn.descriptions"}"#,
        r#"{"id":3,"method":"txn.descriptions","params":{"prefix":"anything"}}"#,
    ]);
    assert!(err_of(&out[1]).is_none(), "nothing to suggest is not a failure: {}", out[1]);
    assert_eq!(out[1]["result"].as_array().unwrap().len(), 0);
    assert_eq!(out[2]["result"].as_array().unwrap().len(), 0);
}

/// A chain is the entry form's answer to money that passes through somewhere on the way: every
/// hop must land as an ordinary payment with its own date and amount, sharing the description, and
/// the hops must be threaded together so the payments panel can follow them and say where the
/// money ended up.
#[test]
fn a_payment_chain_records_every_hop_as_linked_payments() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Sam","kind":"asset"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Bookmaker","kind":"asset"}}"#,
        r#"{"id":4,"method":"account.create","params":{"name":"Wallet","kind":"asset"}}"#,
        // Two hops, a day apart, and Sam keeps 100 of it.
        r#"{"id":5,"method":"txn.create_chain","params":{"description":"stake via Sam","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-02","amount_minor":5000},
                     {"to_account":3,"occurred_on":"2026-09-03","amount_minor":4900}]}}"#,
        r#"{"id":6,"method":"txn.get","params":{"id":1}}"#,
        r#"{"id":7,"method":"txn.get","params":{"id":2}}"#,
        r#"{"id":8,"method":"link.chain","params":{"txn_id":2}}"#,
        // Three hops straight through two stops.
        r#"{"id":9,"method":"txn.create_chain","params":{"description":"round the houses","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-04","amount_minor":700},
                     {"to_account":4,"occurred_on":"2026-09-04","amount_minor":700},
                     {"to_account":3,"occurred_on":"2026-09-04","amount_minor":700}]}}"#,
        r#"{"id":10,"method":"link.chain","params":{"txn_id":4}}"#,
        r#"{"id":11,"method":"account.balances"}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "no step may fail: {r}");
    }
    assert_eq!(out[4]["result"]["txn_ids"], serde_json::json!([1, 2]));

    for (hop, (on, from, to, amount)) in [(&out[5], ("2026-09-02", 1, 2, 5000)), (&out[6], ("2026-09-03", 2, 3, 4900))] {
        let t = &hop["result"];
        assert_eq!(t["description"], "stake via Sam", "the description is copied to every hop");
        assert_eq!(t["occurred_on"], on, "each hop keeps its own date");
        let p = t["postings"].as_array().unwrap();
        assert_eq!((p[0]["account_id"].as_i64(), p[0]["amount_minor"].as_i64()), (Some(from), Some(-amount)));
        assert_eq!((p[1]["account_id"].as_i64(), p[1]["amount_minor"].as_i64()), (Some(to), Some(amount)));
    }

    // Followed from the far end, the thread is the whole chain, drawn hop to hop.
    let thread = &out[7]["result"];
    assert_eq!(thread["nodes"].as_array().unwrap().len(), 2, "{thread}");
    assert_eq!(thread["edges"], serde_json::json!([{ "from": 1, "to": 2, "note": null }]));
    let left: Vec<(String, i64)> = thread["residual"].as_array().unwrap().iter()
        .map(|r| (r["account"].as_str().unwrap().to_string(), r["amount_minor"].as_i64().unwrap())).collect();
    assert!(left.contains(&("Sam".to_string(), 100)), "what Sam kept is what did not pass through: {left:?}");

    // The middle payment of a three-hop chain is threaded both ways; the stops it passed
    // straight through drop out of the residual.
    let thread = &out[9]["result"];
    assert_eq!(thread["nodes"].as_array().unwrap().len(), 3, "{thread}");
    assert_eq!(thread["edges"].as_array().unwrap().len(), 2);
    let middle = thread["nodes"].as_array().unwrap().iter().find(|n| n["id"] == 4).unwrap();
    assert_eq!(middle["links"], 2, "the browser marks the middle hop as threaded twice");
    let mut left: Vec<&str> = thread["residual"].as_array().unwrap().iter()
        .map(|r| r["account"].as_str().unwrap()).collect();
    left.sort();
    assert_eq!(left, vec!["Bookmaker", "Current"], "{thread}");

    let balance = |id: i64| out[10]["result"].as_array().unwrap().iter()
        .find(|a| a["account_id"] == id).map(|a| a["balance_minor"].as_i64().unwrap());
    assert_eq!((balance(1), balance(2), balance(3), balance(4)), (Some(-5700), Some(100), Some(5600), Some(0)));
}

/// Half a chain is worse than none: the money would appear to have stopped somewhere it never
/// was. Most refusals happen before anything is written; the last case here fails on its SECOND
/// hop, after the first is already in the transaction, and must still leave the book untouched.
#[test]
fn a_chain_is_refused_whole_when_any_hop_is_wrong() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Sam","kind":"asset"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Euro wallet","kind":"asset","currency":"EUR"}}"#,
        r#"{"id":4,"method":"account.create","params":{"name":"Groceries","kind":"expense"}}"#,
        r#"{"id":5,"method":"txn.create_chain","params":{"description":"too short","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-02","amount_minor":100}]}}"#,
        r#"{"id":6,"method":"txn.create_chain","params":{"description":"self hop","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-02","amount_minor":100},
                     {"to_account":2,"occurred_on":"2026-09-02","amount_minor":100}]}}"#,
        r#"{"id":7,"method":"txn.create_chain","params":{"description":"nothing moves","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-02","amount_minor":100},
                     {"to_account":1,"occurred_on":"2026-09-02","amount_minor":0}]}}"#,
        r#"{"id":8,"method":"txn.create_chain","params":{"description":"crosses a currency","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-02","amount_minor":100},
                     {"to_account":3,"occurred_on":"2026-09-02","amount_minor":100}]}}"#,
        r#"{"id":9,"method":"txn.create_chain","params":{"description":"unknown stop","from_account":1,
             "hops":[{"to_account":99,"occurred_on":"2026-09-02","amount_minor":100},
                     {"to_account":2,"occurred_on":"2026-09-02","amount_minor":100}]}}"#,
        r#"{"id":10,"method":"txn.create_chain","params":{"description":"through the shopping","from_account":1,
             "hops":[{"to_account":4,"occurred_on":"2026-09-02","amount_minor":100},
                     {"to_account":2,"occurred_on":"2026-09-02","amount_minor":100}]}}"#,
        r#"{"id":11,"method":"txn.create_chain","params":{"description":"second hop on no such day","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-02","amount_minor":100},
                     {"to_account":1,"occurred_on":"2026-02-30","amount_minor":100}]}}"#,
        r#"{"id":12,"method":"txn.create_chain","params":{"description":"arrives before it left","from_account":1,
             "hops":[{"to_account":2,"occurred_on":"2026-09-05","amount_minor":100},
                     {"to_account":1,"occurred_on":"2026-09-01","amount_minor":100}]}}"#,
        r#"{"id":13,"method":"txn.create_chain","params":{"description":"old shape","occurred_on":"2026-09-02",
             "amount_minor":100,"accounts":[1,2,1]}}"#,
        r#"{"id":14,"method":"txn.list"}"#,
    ]);
    for (i, reason) in [(4, "stop"), (5, "itself"), (6, "positive"), (7, "one currency"), (8, "no such account"),
                        (9, "pass through"), (10, "no such day"), (11, "forward in time"), (12, "hops")] {
        let msg = err_of(&out[i]).unwrap_or_else(|| panic!("step {} must be refused: {}", i + 1, out[i]));
        assert!(msg.contains(reason), "the error must say why: {msg}");
    }
    let mismatch = err_of(&out[7]).unwrap();
    assert!(mismatch.contains("Current") && mismatch.contains("Euro wallet"), "both ends of the mismatched leg are named: {mismatch}");
    assert!(err_of(&out[9]).unwrap().contains("Groceries"), "it must name the account money cannot pass through");
    assert!(err_of(&out[10]).unwrap().contains("hop 2"), "it must say which hop: {}", out[10]);
    assert_eq!(out[13]["result"].as_array().unwrap().len(), 0, "nothing was written, not even the first hop");
}

/// A recurring chain is one commitment in several series: every hop projects, the intermediate
/// keeps only what it keeps, the brief counts it once, and ending, renaming, skipping or
/// re-pricing any hop does the same to the chain.
#[test]
fn a_recurring_chain_projects_every_hop_and_behaves_as_one() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Sam","kind":"asset"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Bookmaker","kind":"asset"}}"#,
        r#"{"id":4,"method":"series.create_chain","params":{"description":"stake via Sam",
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=1","dtstart":"2026-10-01","from_account":1,
             "hops":[{"to_account":2,"amount_minor":10000},{"to_account":3,"amount_minor":9500}]}}"#,
        r#"{"id":5,"method":"series.list"}"#,
        r#"{"id":6,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-12-31"}}"#,
        r#"{"id":7,"method":"analysis.brief","params":{"as_of":"2026-09-30","months":3}}"#,
        r#"{"id":8,"method":"series.end","params":{"id":2,"until_on":"2026-11-30"}}"#,
        r#"{"id":9,"method":"series.rename","params":{"id":2,"description":"bets via Sam"}}"#,
        r#"{"id":10,"method":"series.list"}"#,
        r#"{"id":11,"method":"series.override","params":{"series_id":2,"occurrence_on":"2026-10-01","action":"skip"}}"#,
        r#"{"id":12,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-12-31"}}"#,
        r#"{"id":13,"method":"series.override","params":{"series_id":1,"occurrence_on":"2026-11-01","action":"amend","amount_minor":-12000}}"#,
        r#"{"id":14,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-12-31"}}"#,
        r#"{"id":15,"method":"series.clear_override","params":{"series_id":2,"occurrence_on":"2026-11-01"}}"#,
        r#"{"id":16,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-12-31"}}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "no step may fail: {r}");
    }
    assert_eq!(out[3]["result"]["ids"], serde_json::json!([1, 2]));
    assert_eq!(out[3]["result"]["chain_id"], 1);

    let rows = out[4]["result"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0]["chain_id"].as_i64(), rows[0]["chain_seq"].as_i64(), rows[0]["chain_len"].as_i64()), (Some(1), Some(0), Some(2)));
    assert_eq!((rows[1]["chain_id"].as_i64(), rows[1]["chain_seq"].as_i64()), (Some(1), Some(1)));
    assert_eq!((rows[0]["from_account"].as_str(), rows[0]["to_account"].as_str()), (Some("Current"), Some("Sam")));
    assert_eq!((rows[1]["from_account"].as_str(), rows[1]["to_account"].as_str()), (Some("Sam"), Some("Bookmaker")));
    assert_eq!(rows[1]["amount_minor"], -9500, "each hop keeps its own amount");

    // Three months, both hops, two legs each; Sam keeps 5.00 a month; every leg says which it is.
    let proj = &out[5]["result"];
    let occ = proj["occurrences"].as_array().unwrap();
    assert_eq!(occ.iter().filter(|o| o["series_id"] == 1).count(), 6);
    assert_eq!(occ.iter().filter(|o| o["series_id"] == 2).count(), 6);
    assert!(occ.iter().all(|o| o["chain_len"] == 2), "{occ:?}");
    assert!(occ.iter().any(|o| o["series_id"] == 2 && o["chain_seq"] == 1));
    let closing = |p: &serde_json::Value, id: i64| p["balances"].as_array().unwrap().iter()
        .filter(|b| b["account_id"] == id).last().map(|b| b["balance_minor"].as_i64().unwrap());
    assert_eq!((closing(proj, 1), closing(proj, 2), closing(proj, 3)), (Some(-30000), Some(1500), Some(28500)));

    // The brief sees one commitment, leaving Current, not two.
    let commitments = out[6]["result"]["commitments"]["series"].as_array().unwrap();
    let ours: Vec<&serde_json::Value> = commitments.iter().filter(|c| c["description"] == "stake via Sam").collect();
    assert_eq!(ours.len(), 1, "{commitments:?}");
    assert_eq!(ours[0]["account"], "Current");
    assert_eq!(ours[0]["monthly_equivalent_minor"], -10000);
    assert_eq!(out[6]["result"]["commitments"]["monthly_equivalent_by_currency"][0]["amount_minor"], -10000,
               "the household total counts the stake once");

    // Ending and renaming hop 2 did it to hop 1 as well.
    assert_eq!(out[7]["result"]["applied_to"], serde_json::json!([1, 2]));
    let rows = out[9]["result"].as_array().unwrap();
    assert!(rows.iter().all(|r| r["until_on"] == "2026-11-30" && r["description"] == "bets via Sam"), "{rows:?}");

    // Skipping October on hop 2 skipped it on hop 1; November survives.
    let occ = out[11]["result"]["occurrences"].as_array().unwrap();
    assert!(!occ.iter().any(|o| o["occurrence_on"] == "2026-10-01"), "{occ:?}");
    assert_eq!(occ.iter().filter(|o| o["occurrence_on"] == "2026-11-01").count(), 4);
    assert_eq!(occ.len(), 4, "the end date holds on both hops: {occ:?}");

    // Re-pricing hop 1 to 120.00 carries the +20.00 through: hop 2 becomes 115.00, the fee stays.
    let occ = out[13]["result"]["occurrences"].as_array().unwrap();
    let primary = |series: i64, on: &str, account: i64| occ.iter()
        .find(|o| o["series_id"] == series && o["occurrence_on"] == on && o["account_id"] == account)
        .map(|o| o["amount_minor"].as_i64().unwrap());
    assert_eq!(primary(1, "2026-11-01", 1), Some(-12000));
    assert_eq!(primary(2, "2026-11-01", 2), Some(-11500));
    assert_eq!(primary(2, "2026-11-01", 3), Some(11500));

    // Clearing from hop 2 cleared hop 1 too.
    assert_eq!(out[14]["result"]["cleared"], 2);
    let occ = out[15]["result"]["occurrences"].as_array().unwrap();
    let primary = |series: i64, on: &str, account: i64| occ.iter()
        .find(|o| o["series_id"] == series && o["occurrence_on"] == on && o["account_id"] == account)
        .map(|o| o["amount_minor"].as_i64().unwrap());
    assert_eq!((primary(1, "2026-11-01", 1), primary(2, "2026-11-01", 2)), (Some(-10000), Some(-9500)));
}

/// An amount override is a magnitude on the primary leg in the template's own direction: edited
/// from the receiving account's side it used to flip the payment into income.
#[test]
fn an_amount_override_keeps_the_direction_whichever_leg_it_came_from() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Rent","kind":"expense"}}"#,
        r#"{"id":3,"method":"series.create","params":{"description":"Rent","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","postings":[{"account_id":1,"amount_minor":-90000,"role":"primary"},
                                                {"account_id":2,"amount_minor":90000,"role":"balancing"}]}}"#,
        // +95000 is what the editor sends when the Rent leg (a positive one) was the row clicked.
        r#"{"id":4,"method":"series.override","params":{"series_id":1,"occurrence_on":"2026-10-01","action":"amend","amount_minor":95000}}"#,
        r#"{"id":5,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-10-31"}}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "no step may fail: {r}");
    }
    let occ = out[4]["result"]["occurrences"].as_array().unwrap();
    let on_current = occ.iter().find(|o| o["account_id"] == 1).unwrap()["amount_minor"].as_i64().unwrap();
    assert_eq!(on_current, -95000, "rent still leaves the current account: {occ:?}");
}

/// Cancelling any hop of a chain in a scenario cancels the chain: the hops are one commitment.
#[test]
fn cancelling_one_hop_of_a_recurring_chain_in_a_scenario_cancels_the_chain() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Sam","kind":"asset"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Bookmaker","kind":"asset"}}"#,
        r#"{"id":4,"method":"series.create_chain","params":{"description":"stake via Sam",
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=1","dtstart":"2026-10-01","from_account":1,
             "hops":[{"to_account":2,"amount_minor":10000},{"to_account":3,"amount_minor":10000}]}}"#,
        r#"{"id":5,"method":"scenario.create","params":{"name":"no bets"}}"#,
        // Cancelling by the second leg, or replacing a leg, is refused: a chain is cancelled whole,
        // from its first leg.
        r#"{"id":6,"method":"series.create","params":{"description":"cancelled: stake via Sam",
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=1","dtstart":"2026-10-01","scenario_id":1,"supersedes_id":2,"postings":[]}}"#,
        r#"{"id":7,"method":"series.create","params":{"description":"cheaper leg","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","scenario_id":1,"supersedes_id":1,
             "postings":[{"account_id":1,"amount_minor":-5000,"role":"primary"},{"account_id":2,"amount_minor":5000,"role":"balancing"}]}}"#,
        r#"{"id":8,"method":"series.create","params":{"description":"cancelled: stake via Sam",
             "rrule":"FREQ=MONTHLY;BYMONTHDAY=1","dtstart":"2026-10-01","scenario_id":1,"supersedes_id":1,"postings":[]}}"#,
        r#"{"id":9,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-12-31","scenarios":[1]}}"#,
        r#"{"id":10,"method":"forecast.project","params":{"as_of":"2026-09-30","horizon":"2026-12-31"}}"#,
        r#"{"id":11,"method":"scenario.list"}"#,
    ]);
    assert!(err_of(&out[5]).unwrap().contains("first leg"), "{}", out[5]);
    assert!(err_of(&out[6]).unwrap().contains("cancelled"), "{}", out[6]);
    for r in [&out[7], &out[8], &out[9], &out[10]] {
        assert!(err_of(r).is_none(), "no step may fail: {r}");
    }
    let with = out[8]["result"]["occurrences"].as_array().unwrap();
    assert!(!with.iter().any(|o| o["series_id"] == 1 || o["series_id"] == 2), "both hops must go: {with:?}");
    let without = out[9]["result"]["occurrences"].as_array().unwrap();
    assert_eq!(without.iter().filter(|o| o["series_id"] == 1 || o["series_id"] == 2).count(), 12);
    let scenario = &out[10]["result"][0];
    assert_eq!(scenario["supersedes_count"], 1, "{scenario}");
}

#[test]
fn a_recurring_chain_is_refused_whole_when_wrong() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Sam","kind":"asset"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Groceries","kind":"expense"}}"#,
        r#"{"id":4,"method":"account.create","params":{"name":"Euro wallet","kind":"asset","currency":"EUR"}}"#,
        r#"{"id":5,"method":"series.create_chain","params":{"description":"x","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","from_account":1,"hops":[{"to_account":2,"amount_minor":100}]}}"#,
        r#"{"id":6,"method":"series.create_chain","params":{"description":"x","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","from_account":1,"hops":[{"to_account":2,"amount_minor":100},{"to_account":2,"amount_minor":100}]}}"#,
        r#"{"id":7,"method":"series.create_chain","params":{"description":"x","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","from_account":1,"hops":[{"to_account":3,"amount_minor":100},{"to_account":2,"amount_minor":100}]}}"#,
        r#"{"id":8,"method":"series.create_chain","params":{"description":"x","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","from_account":1,"hops":[{"to_account":2,"amount_minor":100},{"to_account":4,"amount_minor":100}]}}"#,
        r#"{"id":9,"method":"series.create_chain","params":{"description":"x","rrule":"FREQ=NOPE",
             "dtstart":"2026-10-01","from_account":1,"hops":[{"to_account":2,"amount_minor":100},{"to_account":1,"amount_minor":100}]}}"#,
        r#"{"id":10,"method":"series.create_chain","params":{"description":"x","rrule":"FREQ=MONTHLY;BYMONTHDAY=1",
             "dtstart":"2026-10-01","from_account":1,"hops":[{"to_account":2,"amount_minor":100},{"to_account":1,"amount_minor":0}]}}"#,
        r#"{"id":11,"method":"series.list"}"#,
    ]);
    for (i, reason) in [(4, "stop"), (5, "itself"), (6, "pass through"), (7, "one currency"), (9, "positive")] {
        let msg = err_of(&out[i]).unwrap_or_else(|| panic!("step {} must be refused: {}", i + 1, out[i]));
        assert!(msg.contains(reason), "the error must say why: {msg}");
    }
    assert_eq!(out[8]["error"]["code"], "bad_rule", "{}", out[8]);
    assert_eq!(out[10]["result"].as_array().unwrap().len(), 0, "nothing was created");
}


/// The chart's history is a point per day an account moved, starting from the balance the window
/// inherited; what happened after the window's end is not counted anywhere.
#[test]
fn balance_history_starts_from_what_the_window_inherited() {
    let out = run(&[
        r#"{"id":1,"method":"account.create","params":{"name":"Current","kind":"asset"}}"#,
        r#"{"id":2,"method":"account.create","params":{"name":"Salary","kind":"income"}}"#,
        r#"{"id":3,"method":"account.create","params":{"name":"Sam","kind":"asset"}}"#,
        r#"{"id":4,"method":"txn.create","params":{"occurred_on":"2026-06-01","description":"pay",
             "postings":[{"account_id":1,"amount_minor":100000},{"account_id":2,"amount_minor":-100000}]}}"#,
        r#"{"id":5,"method":"txn.create","params":{"occurred_on":"2026-07-15","description":"lend",
             "postings":[{"account_id":1,"amount_minor":-20000},{"account_id":3,"amount_minor":20000}]}}"#,
        r#"{"id":6,"method":"txn.create","params":{"occurred_on":"2026-08-01","description":"lend more",
             "postings":[{"account_id":1,"amount_minor":-30000},{"account_id":3,"amount_minor":30000}]}}"#,
        r#"{"id":7,"method":"txn.create","params":{"occurred_on":"2026-09-05","description":"after the window",
             "postings":[{"account_id":1,"amount_minor":-1000},{"account_id":3,"amount_minor":1000}]}}"#,
        r#"{"id":8,"method":"account.history","params":{"from":"2026-07-01","to":"2026-08-31"}}"#,
        r#"{"id":9,"method":"account.history","params":{"from":"2026-07-01","to":"2026-08-31","account_ids":[3]}}"#,
        // A movement ON the window's first day is a point, not part of the opening.
        r#"{"id":10,"method":"account.history","params":{"from":"2026-07-15","to":"2026-08-31","account_ids":[1]}}"#,
        // No window at all: nothing is carried in, everything is a point.
        r#"{"id":11,"method":"account.history","params":{"account_ids":[1]}}"#,
    ]);
    for r in &out {
        assert!(err_of(r).is_none(), "no step may fail: {r}");
    }
    let rows = out[7]["result"].as_array().unwrap();
    let current = rows.iter().find(|a| a["name"] == "Current").unwrap();
    assert_eq!(current["opening_minor"], 100000, "June's pay is carried in, not drawn");
    assert_eq!(current["points"], serde_json::json!([
        { "on": "2026-07-15", "balance_minor": 80000 },
        { "on": "2026-08-01", "balance_minor": 50000 },
    ]), "one point per day it moved, at the closing balance");

    let on_the_day = &out[9]["result"][0];
    assert_eq!(on_the_day["opening_minor"], 100000);
    assert_eq!(on_the_day["points"][0], serde_json::json!({ "on": "2026-07-15", "balance_minor": 80000 }));
    let whole = &out[10]["result"][0];
    assert_eq!(whole["opening_minor"], 0);
    assert_eq!(whole["points"].as_array().unwrap().len(), 4, "{whole}");
    let salary = rows.iter().find(|a| a["name"] == "Salary").unwrap();
    assert_eq!((salary["opening_minor"].as_i64(), salary["points"].as_array().unwrap().len()), (Some(-100000), 0));

    let only_sam = out[8]["result"].as_array().unwrap();
    assert_eq!(only_sam.len(), 1);
    assert_eq!(only_sam[0]["points"].as_array().unwrap().len(), 2, "{only_sam:?}");
}

// ---------------------------------------------------------------------------------------------
// Reviewing and editing recurring payment rules (series.review, series.revise, series.undo_change)
// ---------------------------------------------------------------------------------------------

use serde_json::{json, Value};

/// A script of calls against one fresh book, numbered in order.
struct Script {
    lines: Vec<String>,
    next: i64,
}

impl Script {
    fn new() -> Self {
        Script { lines: Vec::new(), next: 1 }
    }
    fn call(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next;
        self.next += 1;
        self.lines.push(json!({ "id": id, "method": method, "params": params }).to_string());
        id
    }
    fn account(&mut self, name: &str, kind: &str) -> i64 {
        self.call("account.create", json!({ "name": name, "kind": kind }))
    }
    fn run(&self) -> Replies {
        let refs: Vec<&str> = self.lines.iter().map(String::as_str).collect();
        Replies(run(&refs))
    }
}

struct Replies(Vec<Value>);

impl Replies {
    fn get(&self, id: i64) -> &Value {
        self.0.iter().find(|r| r["id"] == id).unwrap_or_else(|| panic!("no reply for {id}"))
    }
    #[track_caller]
    fn ok(&self, id: i64) -> &Value {
        let r = self.get(id);
        assert!(err_of(r).is_none(), "call {id} failed: {r}");
        &r["result"]
    }
    #[track_caller]
    fn err(&self, id: i64) -> (String, String) {
        let r = self.get(id);
        let e = r.get("error").unwrap_or_else(|| panic!("call {id} should have failed: {r}"));
        (e["code"].as_str().unwrap_or("").to_string(), e["message"].as_str().unwrap_or("").to_string())
    }
}

fn plain(desc: &str, from: i64, to: i64, amount: i64, rrule: &str, start: &str) -> Value {
    json!({
        "description": desc, "rrule": rrule, "dtstart": start,
        "postings": [
            { "account_id": from, "amount_minor": -amount, "role": "primary" },
            { "account_id": to, "amount_minor": amount, "role": "balancing" },
        ],
    })
}

/// (series_id, occurrence_on, value_on, amount) of every series occurrence on `account`.
fn legs(projection: &Value, account: i64) -> Vec<(i64, String, String, i64)> {
    projection["occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["kind"] == "series" && o["account_id"] == account)
        .map(|o| {
            (
                o["series_id"].as_i64().unwrap(),
                o["occurrence_on"].as_str().unwrap().to_string(),
                o["value_on"].as_str().unwrap().to_string(),
                o["amount_minor"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn rule<'a>(review: &'a Value, desc: &str) -> &'a Value {
    review["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["description"] == desc)
        .unwrap_or_else(|| panic!("no rule {desc} in {review}"))
}

const AS_OF: &str = "2026-09-14";

#[test]
fn the_review_lists_every_rule_with_what_hangs_off_it() {
    let mut s = Script::new();
    let current = s.account("Current", "asset");
    let rent = s.account("Rent", "expense");
    let gym = s.account("Gym", "expense");
    let sam = s.account("Sam", "asset");
    let bookie = s.account("Bookmaker", "asset");
    let old_bank = s.account("Old bank", "asset");
    let card = s.account("Card", "liability");
    let interest = s.account("Interest", "expense");
    let fees = s.account("Fees", "expense");
    let phone = s.account("Phone", "expense");
    s.call("series.create", plain("Rent", current, rent, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.create_chain", json!({
        "description": "stake via Sam", "rrule": "FREQ=MONTHLY;BYMONTHDAY=1", "dtstart": "2026-10-01",
        "from_account": current, "hops": [{ "to_account": sam, "amount_minor": 10000 }, { "to_account": bookie, "amount_minor": 9500 }],
    }));
    let mut gym_rule = plain("Gym", old_bank, gym, 3000, "FREQ=MONTHLY;BYMONTHDAY=10", "2026-01-10");
    gym_rule["until_on"] = json!("2026-06-10");
    s.call("series.create", gym_rule);
    s.call("series.create", json!({
        "description": "Phone", "rrule": "FREQ=MONTHLY;BYMONTHDAY=20", "dtstart": "2026-01-20",
        "postings": [
            { "account_id": current, "amount_minor": -5000, "role": "primary" },
            { "account_id": phone, "amount_minor": 4000, "role": "balancing" },
            { "account_id": fees, "amount_minor": 1000, "role": "other" },
        ],
    }));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-09-01" }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-10-01", "action": "skip" }));
    let change = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    s.call("scenario.create", json!({ "name": "move out" }));
    let mut dream = plain("Dream rent", current, rent, 150000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-11-01");
    dream["scenario_id"] = json!(1);
    s.call("series.create", dream);
    s.call("series.create", json!({
        "description": "cancel Gym", "rrule": "FREQ=MONTHLY;BYMONTHDAY=10", "dtstart": "2026-01-10",
        "scenario_id": 1, "supersedes_id": 4, "postings": [],
    }));
    s.call("account.close", json!({ "id": old_bank }));
    s.call("interest.create_rule", json!({
        "account_id": card, "counter_account_id": interest, "shape": "revolving", "quoted_rate": "24.9",
        "accrual_freq": "daily", "capitalise_dtstart": "2026-01-01", "grace_period": true,
    }));
    s.call("payment.create_rule", json!({
        "account_id": card, "from_account_id": current, "amount_kind": "pct_of_statement", "pct": "1.0",
        "floor_minor": 500, "rrule": "FREQ=MONTHLY;BYMONTHDAY=15", "dtstart": "2026-01-15",
        "interest_rule_id": 1, "due_offset_days": 21,
    }));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let check = s.call("db.check", json!({}));
    let out = s.run();
    for r in &out.0 {
        assert!(err_of(r).is_none(), "no step may fail: {r}");
    }
    assert_eq!(out.ok(change)["mode"], "split");
    let v = out.ok(review);
    assert_eq!(v["rules"].as_array().unwrap().len(), 4, "{v}");

    let r = rule(v, "Rent");
    assert_eq!(r["lineage_id"], 1);
    assert_eq!(r["parts"].as_array().unwrap().len(), 2);
    assert_eq!(r["parts"][0]["until_on"], "2026-12-31");
    assert_eq!(r["current"]["ids"], json!([6]));
    assert_eq!(r["recorded"]["count"], 1);
    assert_eq!(r["recorded"]["last"]["occurrence_on"], "2026-09-01");
    assert_eq!(r["recorded"]["last"]["amount_minor"], -90000);
    assert!(r["overrides"].as_array().unwrap().iter().any(|o| o["occurrence_on"] == "2026-10-01" && o["action"] == "skip"));
    assert_eq!(r["next"][0]["occurrence_on"], "2026-11-01", "October is skipped");
    assert_eq!(r["next_12m"]["count"], 11);
    assert_eq!(r["next_12m"]["total_minor"], -(2 * 90000 + 9 * 95000));
    assert_eq!(r["group"], "running");
    assert_eq!(r["direction"], "out");
    assert_eq!(r["current"]["phrase"], "monthly on the 1st");
    assert_eq!((r["editable"]["from"].as_bool(), r["editable"]["money"].as_bool()), (Some(true), Some(true)));

    let chain = rule(v, "stake via Sam");
    assert_eq!(chain["chain_len"], 2);
    let route: Vec<i64> = chain["current"]["route"].as_array().unwrap().iter().map(|a| a["account_id"].as_i64().unwrap()).collect();
    assert_eq!(route, vec![current, sam, bookie]);
    let amounts: Vec<i64> = chain["current"]["hops"].as_array().unwrap().iter().map(|h| h["amount_minor"].as_i64().unwrap()).collect();
    assert_eq!(amounts, vec![10000, 9500]);
    assert_eq!(chain["group"], "not_started");
    assert_eq!(chain["direction"], "transfer");

    let g = rule(v, "Gym");
    assert_eq!(g["group"], "ended");
    assert_eq!(g["current"]["route"][0]["closed"], true);
    assert_eq!(g["cancelled_in"][0]["scenario"], "move out");

    let p = rule(v, "Phone");
    assert_eq!(p["shape"], "custom");
    assert_eq!(p["editable"]["money"], false);
    assert!(p["editable"]["reason"].as_str().unwrap().contains("3 legs"), "{}", p["editable"]);

    assert_eq!(v["what_if"][0]["rules"][0]["description"], "Dream rent");
    assert_eq!(v["what_if"][0]["rules"][0]["editable"]["from"], false);
    assert_eq!(v["what_if"][0]["cancels"][0]["target"], "Gym");
    assert_eq!(v["card_rules"].as_array().unwrap().len(), 1);
    assert_eq!(v["card_rules"][0]["amount_kind"], "pct_of_statement");
    assert_eq!(v["card_rules"][0]["pct"], "1");
    assert_eq!(v["interest_rules"][0]["shape"], "revolving");
    assert_eq!(v["interest_rules"][0]["rate_in_force"]["quoted"], "24.9");
    let t = &v["totals"];
    assert_eq!((t["running"].as_i64(), t["not_started"].as_i64(), t["ended"].as_i64(), t["what_if"].as_i64(), t["cancels"].as_i64(), t["changed"].as_i64()),
               (Some(2), Some(1), Some(1), Some(1), Some(1), Some(1)));
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn a_rule_can_be_repriced_in_place_without_touching_what_it_already_paid() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-09-01", "amount_minor": 88000 }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-11-01", "moved_to": "2026-11-03", "amount_minor": -90000 }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-12-01", "amount_minor": -120000 }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2027-02-01", "amount_minor": -90000 }));
    let window = json!({ "as_of": AS_OF, "horizon": "2027-02-28" });
    let p0 = s.call("forecast.project", window.clone());
    let edit = json!({ "id": 1, "as_of": AS_OF, "mode": "whole", "amounts": [95000], "amends": "follow" });
    let mut dry = edit.clone();
    dry["dry_run"] = json!(true);
    let d = s.call("series.revise", dry);
    let p1 = s.call("forecast.project", window.clone());
    let real = s.call("series.revise", edit);
    let txn = s.call("txn.get", json!({ "id": 1 }));
    let p2 = s.call("forecast.project", window);
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let list = s.call("series.list", json!({}));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    let d = out.ok(d);
    assert_eq!((d["written"].as_bool(), d["mode"].as_str()), (Some(false), Some("in_place")));
    assert_eq!(d["changed"], json!(["amounts"]));
    assert_eq!(d["amends"]["follow"], json!([
        { "occurrence_on": "2026-11-01", "removed": false },
        { "occurrence_on": "2027-02-01", "removed": true },
    ]), "only amounts saved at the old price follow the new one");
    assert_eq!(d["amends"]["keep"], json!([{ "occurrence_on": "2026-12-01", "amounts": [120000] }]));
    assert_eq!(out.ok(p0), out.ok(p1), "a dry run writes nothing");
    let r = out.ok(real);
    assert_eq!(r["written"], true);
    assert_eq!(r["impact"], d["impact"], "the save does exactly what the dry run said");
    assert_eq!(r["amends"]["follow"], d["amends"]["follow"]);

    let t = out.ok(txn);
    assert_eq!(t["description"], "Rent");
    let legs_of_txn: Vec<(i64, i64)> = t["postings"].as_array().unwrap().iter().map(|p| (p["account_id"].as_i64().unwrap(), p["amount_minor"].as_i64().unwrap())).collect();
    assert!(legs_of_txn.contains(&(1, -88000)) && legs_of_txn.contains(&(2, 88000)), "the recorded payment keeps what it recorded: {t}");

    let rent: Vec<(String, String, i64)> = legs(out.ok(p2), 1).into_iter().filter(|l| l.0 == 1).map(|l| (l.1, l.2, l.3)).collect();
    let day = |a: &str, b: &str, m: i64| (a.to_string(), b.to_string(), m);
    assert_eq!(rent, vec![
        day("2026-10-01", "2026-10-01", -95000),
        day("2026-11-01", "2026-11-03", -95000),
        day("2026-12-01", "2026-12-01", -120000),
        day("2027-01-01", "2027-01-01", -95000),
        day("2027-02-01", "2027-02-01", -95000),
    ]);
    let ov = rule(out.ok(review), "Rent")["overrides"].as_array().unwrap().clone();
    let nov = ov.iter().find(|o| o["occurrence_on"] == "2026-11-01").unwrap();
    assert_eq!((nov["amounts"].clone(), nov["moved_to"].clone()), (json!([null]), json!("2026-11-03")));
    assert!(!ov.iter().any(|o| o["occurrence_on"] == "2027-02-01"), "an amend left with nothing is removed");
    let rows = out.ok(list).as_array().unwrap().clone();
    assert_eq!((rows.len(), rows[0]["amount_minor"].as_i64()), (1, Some(-95000)));
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn moving_a_rules_accounts_keeps_its_currency_and_its_records() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.account("Savings", "asset");
    s.call("account.create", json!({ "name": "Euro", "kind": "asset", "currency": "EUR" }));
    s.account("Old", "asset");
    s.call("account.close", json!({ "id": 5 }));
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-09-01" }));
    let revise = |s: &mut Script, route: Value, amounts: Option<Value>| {
        let mut p = json!({ "id": 1, "as_of": AS_OF, "mode": "whole", "route": route });
        if let Some(a) = amounts {
            p["amounts"] = a;
        }
        s.call("series.revise", p)
    };
    let moved = revise(&mut s, json!([3, 2]), None);
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-10-31" }));
    let txn = s.call("txn.get", json!({ "id": 1 }));
    let euro = revise(&mut s, json!([4, 2]), None);
    let itself = revise(&mut s, json!([3, 3]), None);
    let closed = revise(&mut s, json!([5, 2]), None);
    let stops = revise(&mut s, json!([1, 2, 3]), None);
    let zero = revise(&mut s, json!([3, 2]), Some(json!([0])));
    let again = revise(&mut s, json!([3, 2]), None);
    let list = s.call("series.list", json!({}));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    assert_eq!(out.ok(moved)["mode"], "in_place");
    let oct: Vec<(i64, i64)> = out.ok(fc)["occurrences"].as_array().unwrap().iter()
        .filter(|o| o["kind"] == "series" && o["occurrence_on"] == "2026-10-01")
        .map(|o| (o["account_id"].as_i64().unwrap(), o["amount_minor"].as_i64().unwrap())).collect();
    assert_eq!(oct, vec![(2, 90000), (3, -90000)]);
    assert!(out.ok(txn)["postings"].as_array().unwrap().iter().any(|p| p["account_id"] == 1), "the record stays on Current");
    let (code, msg) = out.err(euro);
    assert_eq!(code, "currency_change");
    assert!(msg.contains("GBP") && msg.contains("EUR"), "{msg}");
    let (code, msg) = out.err(itself);
    assert!(code == "bad_params" && msg.contains("itself"), "{code}: {msg}");
    assert_eq!(out.err(closed).0, "closed_account");
    let (code, msg) = out.err(stops);
    assert!(code == "bad_chain" && msg.contains("stops"), "{code}: {msg}");
    let (code, msg) = out.err(zero);
    assert!(code == "bad_params" && msg.contains("positive"), "{code}: {msg}");
    assert_eq!((out.ok(again)["mode"].as_str(), out.ok(again)["written"].as_bool()), (Some("unchanged"), Some(false)));
    assert_eq!(out.ok(list)[0]["from_account"], "Savings");
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn a_schedule_change_for_the_whole_rule_is_refused_while_a_date_hangs_off_it() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-09-01" }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-11-01", "action": "skip" }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-06-01", "action": "skip" }));
    let window = json!({ "as_of": AS_OF, "horizon": "2026-12-31" });
    let p0 = s.call("forecast.project", window.clone());
    let fifth = json!({ "id": 1, "as_of": AS_OF, "mode": "whole", "rrule": "FREQ=MONTHLY;BYMONTHDAY=5" });
    let mut dry = fifth.clone();
    dry["dry_run"] = json!(true);
    let d = s.call("series.revise", dry);
    let real = s.call("series.revise", fifth);
    let list = s.call("series.list", json!({}));
    let p1 = s.call("forecast.project", window.clone());
    let split = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2026-09-02", "rrule": "FREQ=MONTHLY;BYMONTHDAY=5" }));
    let p2 = s.call("forecast.project", window);
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    let d = out.ok(d);
    assert_eq!((d["ok"].as_bool(), d["refusal"]["code"].as_str()), (Some(false), Some("slots_detached")));
    let blockers: Vec<(String, String, bool)> = d["blockers"].as_array().unwrap().iter()
        .map(|b| (b["kind"].as_str().unwrap().to_string(), b["occurrence_on"].as_str().unwrap().to_string(), b["droppable"].as_bool().unwrap()))
        .collect();
    assert_eq!(blockers, vec![("recorded".into(), "2026-09-01".into(), false), ("override".into(), "2026-11-01".into(), true)],
               "a skip older than the lookback changes no forecast and blocks nothing");
    assert_eq!(d["suggest_from"], "2026-09-02");
    let (code, msg) = out.err(real);
    assert_eq!(code, "slots_detached");
    assert!(msg.contains("1/9/2026") && msg.contains("2/9/2026"), "{msg}");
    assert_eq!(out.ok(list)[0]["rrule"], "FREQ=MONTHLY;BYMONTHDAY=1");
    assert_eq!(out.ok(p0), out.ok(p1), "a refused save writes nothing");

    let sp = out.ok(split);
    assert_eq!(sp["mode"], "split");
    assert_eq!(sp["ended"], json!({ "ids": [1], "until_on": "2026-09-01" }));
    assert_eq!(sp["created"]["ids"], json!([2]));
    assert_eq!(sp["created"]["dtstart"], "2026-09-05");
    assert_eq!(sp["created"]["head_ids"], json!([1]));
    assert_eq!(sp["left_behind"][0]["occurrence_on"], "2026-11-01");
    assert_eq!(sp["anchor"]["rrule_unchanged"], false);
    let rent: Vec<(i64, String)> = legs(out.ok(p2), 1).into_iter().map(|l| (l.0, l.1)).collect();
    assert_eq!(rent, vec![(2, "2026-10-05".into()), (2, "2026-11-05".into()), (2, "2026-12-05".into())]);
    let r = rule(out.ok(review), "Rent");
    assert_eq!(r["parts"].as_array().unwrap().len(), 2);
    assert_eq!(r["parts"][0]["left_behind"][0]["occurrence_on"], "2026-11-01");
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn a_schedule_change_in_place_goes_through_when_every_date_survives_and_drops_only_when_confirmed() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Gym", "expense");
    s.call("series.create", plain("Gym", 1, 2, 1000, "FREQ=WEEKLY;BYDAY=FR", "2026-08-07"));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-09-18", "action": "skip" }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-09-25", "amount_minor": -1500 }));
    let fortnightly = json!({ "id": 1, "as_of": AS_OF, "mode": "whole", "rrule": "FREQ=WEEKLY;INTERVAL=2;BYDAY=FR" });
    let mut dry = fortnightly.clone();
    dry["dry_run"] = json!(true);
    let d = s.call("series.revise", dry);
    let refused = s.call("series.revise", fortnightly.clone());
    let mut confirmed = fortnightly;
    confirmed["drop_overrides"] = json!(true);
    let saved = s.call("series.revise", confirmed);
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-10-31" }));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let bad_rule = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "rrule": "FREQ=NOPE" }));
    let count = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "rrule": "FREQ=MONTHLY;COUNT=3" }));
    let hourly = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "rrule": "FREQ=HOURLY" }));
    let empty = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "rrule": "FREQ=MONTHLY;BYMONTHDAY=31", "until_on": "2026-08-20" }));
    let still = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-10-31" }));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    let d = out.ok(d);
    let blockers = d["blockers"].as_array().unwrap();
    assert_eq!(blockers.len(), 1, "{d}");
    assert_eq!((blockers[0]["occurrence_on"].as_str(), blockers[0]["droppable"].as_bool()), (Some("2026-09-25"), Some(true)));
    assert_eq!(d["refusal"]["code"], "would_drop");
    let (code, msg) = out.err(refused);
    assert!(code == "would_drop" && msg.contains("25/9/2026"), "{code}: {msg}");
    let saved = out.ok(saved);
    assert_eq!(saved["mode"], "in_place");
    assert_eq!(saved["dropped"][0]["occurrence_on"], "2026-09-25");
    let dates: Vec<String> = legs(out.ok(fc), 1).into_iter().map(|l| l.1).collect();
    assert_eq!(dates, vec!["2026-10-02", "2026-10-16", "2026-10-30"]);
    let ov = &rule(out.ok(review), "Gym")["overrides"];
    assert_eq!(ov.as_array().unwrap().len(), 1);
    assert_eq!((ov[0]["occurrence_on"].as_str(), ov[0]["action"].as_str()), (Some("2026-09-18"), Some("skip")));
    assert_eq!(out.err(bad_rule).0, "bad_rule");
    let (code, msg) = out.err(count);
    assert!(code == "bad_params" && msg.contains("COUNT"), "{code}: {msg}");
    assert_eq!(out.err(hourly).0, "bad_params");
    let (code, msg) = out.err(empty);
    assert!(code == "bad_rule" && msg.contains("no payments"), "{code}: {msg}");
    out.ok(still);
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn changing_a_rule_from_a_date_keeps_its_history_its_adjustments_and_its_rhythm() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.account("Club", "expense");
    s.account("Insurance", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-09-01" }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-12-01", "action": "skip" }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2027-01-01", "moved_to": "2027-01-04", "amount_minor": -90000 }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2027-02-01", "amount_minor": -99900 }));
    s.call("series.create", plain("Club", 1, 3, 2000, "FREQ=WEEKLY;INTERVAL=2;BYDAY=FR", "2026-08-07"));
    s.call("series.create", plain("Insurance", 1, 4, 30000, "FREQ=YEARLY", "2026-03-05"));
    let rent = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000], "amends": "follow" }));
    let club = s.call("series.revise", json!({ "id": 2, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [2500] }));
    let insurance = s.call("series.revise", json!({ "id": 3, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [32000] }));
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2027-03-31" }));
    let txn = s.call("txn.get", json!({ "id": 1 }));
    let brief = s.call("analysis.brief", json!({ "as_of": AS_OF }));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    let r = out.ok(rent);
    assert_eq!(r["mode"], "split");
    assert_eq!(r["ended"]["until_on"], "2026-12-31");
    assert_eq!((r["created"]["ids"].clone(), r["created"]["dtstart"].clone()), (json!([4]), json!("2027-01-01")));
    let carried: Vec<&str> = r["carried"].as_array().unwrap().iter().map(|c| c["occurrence_on"].as_str().unwrap()).collect();
    assert_eq!(carried, vec!["2027-01-01", "2027-02-01"]);
    assert_eq!(r["amends"]["follow"], json!([{ "occurrence_on": "2027-01-01", "removed": false }]));
    assert_eq!(r["amends"]["keep"], json!([{ "occurrence_on": "2027-02-01", "amounts": [99900] }]));
    assert_eq!(out.ok(club)["created"]["dtstart"], "2027-01-08", "a fortnightly rule keeps its own Fridays");
    assert_eq!(out.ok(insurance)["created"]["dtstart"], "2027-03-05", "a yearly rule keeps its anniversary");

    let current = legs(out.ok(fc), 1);
    let at = |sid: i64, on: &str| current.iter().find(|l| l.0 == sid && l.1 == on).map(|l| (l.2.clone(), l.3));
    assert_eq!(at(1, "2026-10-01"), Some(("2026-10-01".into(), -90000)));
    assert_eq!(at(1, "2026-11-01"), Some(("2026-11-01".into(), -90000)));
    assert!(!current.iter().any(|l| l.1 == "2026-12-01"), "December is still skipped");
    assert_eq!(at(4, "2027-01-01"), Some(("2027-01-04".into(), -95000)), "the moved date takes the new price");
    assert_eq!(at(4, "2027-02-01"), Some(("2027-02-01".into(), -99900)), "a price of its own is kept");
    assert_eq!(at(4, "2027-03-01"), Some(("2027-03-01".into(), -95000)));
    assert_eq!(at(2, "2026-12-25").map(|x| x.1), Some(-2000));
    assert_eq!(at(5, "2027-01-08").map(|x| x.1), Some(-2500));
    assert_eq!(at(5, "2027-01-22").map(|x| x.1), Some(-2500));
    assert_eq!(at(6, "2027-03-05").map(|x| x.1), Some(-32000));
    assert!(!current.iter().any(|l| l.0 == 3), "the old insurance part has nothing in the window");
    assert!(out.ok(txn)["postings"].as_array().unwrap().iter().any(|p| p["account_id"] == 1 && p["amount_minor"] == -90000));

    let commitments = out.ok(brief)["commitments"]["series"].as_array().unwrap().clone();
    let rents: Vec<&Value> = commitments.iter().filter(|c| c["description"] == "Rent").collect();
    assert_eq!(rents.len(), 1, "one rule, counted once: {commitments:?}");
    assert_eq!(rents[0]["series_id"], 1);
    assert_eq!(rents[0]["parts"], json!([1, 4]));
    assert_eq!(rents[0]["occurrences_next_12m"], 11);
    assert_eq!(rents[0]["annual_minor"], -1039900);
    let rr = rule(out.ok(review), "Rent");
    assert_eq!((rr["next_12m"]["count"].as_i64(), rr["next_12m"]["total_minor"].as_i64()), (Some(11), Some(-1039900)),
               "the review's figure is the brief's figure");
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn a_change_from_a_date_is_refused_while_a_payment_is_recorded_on_or_after_it() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2027-01-01", "occurred_on": "2026-12-30" }));
    let change = json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] });
    let mut dry = change.clone();
    dry["dry_run"] = json!(true);
    let d = s.call("series.revise", dry);
    let real = s.call("series.revise", change);
    let list = s.call("series.list", json!({}));
    let later = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-02", "amounts": [95000] }));
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2027-03-31" }));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    let d = out.ok(d);
    assert_eq!(d["refusal"]["code"], "recorded_after");
    assert_eq!((d["blockers"][0]["kind"].as_str(), d["blockers"][0]["occurrence_on"].as_str(), d["blockers"][0]["occurred_on"].as_str()),
               (Some("recorded"), Some("2027-01-01"), Some("2026-12-30")));
    assert_eq!(d["suggest_from"], "2027-01-02");
    let (code, msg) = out.err(real);
    assert_eq!(code, "recorded_after");
    assert!(msg.contains("1/1/2027") && msg.contains("30/12/2026") && msg.contains("2/1/2027"), "{msg}");
    let rows = out.ok(list).as_array().unwrap().clone();
    assert_eq!((rows.len(), rows[0]["until_on"].clone()), (1, Value::Null));
    let l = out.ok(later);
    assert_eq!((l["mode"].as_str(), l["ended"]["until_on"].as_str(), l["created"]["dtstart"].as_str()),
               (Some("split"), Some("2027-01-01"), Some("2027-02-01")));
    let occ = out.ok(fc)["occurrences"].as_array().unwrap().clone();
    assert!(!occ.iter().any(|o| o["kind"] == "series" && o["occurrence_on"] == "2027-01-01"), "January is recorded, not projected");
    assert!(occ.iter().any(|o| o["kind"] == "real" && o["value_on"] == "2026-12-30"));
    assert!(occ.iter().any(|o| o["series_id"] == 2 && o["occurrence_on"] == "2027-02-01" && o["account_id"] == 1 && o["amount_minor"] == -95000));
    assert_eq!(rule(out.ok(review), "Rent")["recorded"]["ahead"][0]["occurrence_on"], "2027-01-01");
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn a_rule_not_yet_started_is_corrected_in_place_even_from_a_date() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-10-01"));
    let a = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2026-10-01", "amounts": [95000] }));
    let b = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2026-09-20", "amounts": [96000] }));
    let c = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2026-10-01", "rrule": "FREQ=MONTHLY;BYMONTHDAY=15" }));
    let list = s.call("series.list", json!({}));
    let out = s.run();
    assert_eq!((out.ok(a)["mode"].as_str(), out.ok(a)["applied_to"].clone()), (Some("in_place"), json!([1])));
    assert_eq!(out.ok(b)["mode"], "in_place");
    assert_eq!(out.ok(c)["mode"], "in_place");
    let rows = out.ok(list).as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "nothing of it had happened, so there was no history to keep apart");
    assert_eq!(rows[0]["dtstart"], "2026-10-15");
    assert_eq!(rows[0]["amount_minor"], -96000);
}

#[test]
fn a_change_that_makes_a_past_date_due_needs_acknowledging() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    let mut rent = plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01");
    rent["weekend_rule"] = json!("after");
    s.call("series.create", rent);
    let twelfth = json!({ "id": 1, "as_of": AS_OF, "mode": "whole", "rrule": "FREQ=MONTHLY;BYMONTHDAY=12" });
    let mut dry = twelfth.clone();
    dry["dry_run"] = json!(true);
    let d = s.call("series.revise", dry);
    let refused = s.call("series.revise", twelfth.clone());
    let mut ack = twelfth;
    ack["acknowledge_due"] = json!(true);
    let saved = s.call("series.revise", ack);
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-10-31" }));
    let out = s.run();
    let (code, msg) = out.err(refused);
    assert_eq!(code, "would_fall_due");
    assert!(msg.contains("12/9/2026") && msg.contains("14/9/2026"), "{msg}");
    let b: Vec<(String, String, String)> = out.ok(d)["blockers"].as_array().unwrap().iter()
        .map(|b| (b["kind"].as_str().unwrap().into(), b["occurrence_on"].as_str().unwrap().into(), b["value_on"].as_str().unwrap().into()))
        .collect();
    assert_eq!(b, vec![("due".into(), "2026-09-12".into(), "2026-09-14".into())]);
    out.ok(saved);
    assert!(legs(out.ok(fc), 1).iter().any(|l| l.1 == "2026-09-12" && l.2 == "2026-09-14"));
}

#[test]
fn a_scenario_cancel_still_covers_a_rule_after_it_changes_from_a_date() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("scenario.create", json!({ "name": "move out" }));
    s.call("series.create", json!({ "description": "cancel", "rrule": "FREQ=MONTHLY;BYMONTHDAY=1", "dtstart": "2026-01-01", "scenario_id": 1, "supersedes_id": 1, "postings": [] }));
    let change = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    s.call("scenario.create", json!({ "name": "move later" }));
    s.call("series.create", json!({ "description": "cancel", "rrule": "FREQ=MONTHLY;BYMONTHDAY=1", "dtstart": "2027-01-01", "scenario_id": 2, "supersedes_id": 3, "postings": [] }));
    let window = |sc: Value| json!({ "as_of": AS_OF, "horizon": "2027-03-31", "scenarios": sc });
    let one = s.call("forecast.project", window(json!([1])));
    let two = s.call("forecast.project", window(json!([2])));
    let none = s.call("forecast.project", window(json!([])));
    let scenarios = s.call("scenario.list", json!({}));
    let out = s.run();
    assert_eq!(out.ok(change)["created"]["ids"], json!([3]));
    let has = |v: &Value, sid: i64| v["occurrences"].as_array().unwrap().iter().any(|o| o["series_id"] == sid);
    for p in [out.ok(one), out.ok(two)] {
        assert!(!has(p, 1) && !has(p, 3), "a cancel of either part covers the whole rent");
    }
    assert!(has(out.ok(none), 1) && has(out.ok(none), 3));
    for sc in out.ok(scenarios).as_array().unwrap() {
        assert_eq!(sc["supersedes_count"], 1, "{sc}");
    }
}

#[test]
fn a_recurring_chain_is_edited_as_one_commitment() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Sam", "asset");
    s.account("Bookmaker", "asset");
    s.account("Wallet", "asset");
    s.account("Groceries", "expense");
    s.call("account.create", json!({ "name": "Euro", "kind": "asset", "currency": "EUR" }));
    s.call("series.create_chain", json!({
        "description": "stake via Sam", "rrule": "FREQ=MONTHLY;BYMONTHDAY=1", "dtstart": "2026-10-01",
        "from_account": 1, "hops": [{ "to_account": 2, "amount_minor": 10000 }, { "to_account": 3, "amount_minor": 9500 }],
    }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-12-01", "moved_to": "2026-12-02", "amount_minor": -10000 }));
    let as_of = "2026-09-30";
    let priced = s.call("series.revise", json!({ "id": 2, "as_of": as_of, "mode": "whole", "amounts": [11000, 10500], "amends": "follow" }));
    let fc1 = s.call("forecast.project", json!({ "as_of": as_of, "horizon": "2026-12-31" }));
    let rerouted = s.call("series.revise", json!({ "id": 1, "as_of": as_of, "route": [1, 4, 3] }));
    let fc2 = s.call("forecast.project", json!({ "as_of": as_of, "horizon": "2026-11-30" }));
    let expense_stop = s.call("series.revise", json!({ "id": 1, "as_of": as_of, "route": [1, 5, 3] }));
    let extra_stop = s.call("series.revise", json!({ "id": 1, "as_of": as_of, "route": [1, 2, 4, 3] }));
    let euro_stop = s.call("series.revise", json!({ "id": 1, "as_of": as_of, "route": [1, 6, 3] }));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-10-01", "whole_chain": true }));
    let reschedule = s.call("series.revise", json!({ "id": 1, "as_of": as_of, "rrule": "FREQ=MONTHLY;BYMONTHDAY=2" }));
    let split = s.call("series.revise", json!({ "id": 1, "as_of": as_of, "mode": "from", "from_on": "2026-11-01", "amounts": [12000, 11500] }));
    let fc3 = s.call("forecast.project", json!({ "as_of": as_of, "horizon": "2026-11-30" }));
    let brief = s.call("analysis.brief", json!({ "as_of": as_of }));
    let record = s.call("series.record", json!({ "series_id": 3, "occurrence_on": "2026-11-01", "whole_chain": true }));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    let p = out.ok(priced);
    assert_eq!(p["applied_to"], json!([1, 2]));
    assert_eq!(p["amends"]["follow"], json!([{ "occurrence_on": "2026-12-01", "removed": false }]));
    let first_legs = |v: &Value, on: &str| -> Vec<(i64, i64, i64, String)> {
        v["occurrences"].as_array().unwrap().iter()
            .filter(|o| o["kind"] == "series" && o["occurrence_on"] == on && o["amount_minor"].as_i64().unwrap() < 0)
            .map(|o| (o["series_id"].as_i64().unwrap(), o["account_id"].as_i64().unwrap(), o["amount_minor"].as_i64().unwrap(), o["value_on"].as_str().unwrap().to_string()))
            .collect()
    };
    assert_eq!(first_legs(out.ok(fc1), "2026-10-01"), vec![(1, 1, -11000, "2026-10-01".into()), (2, 2, -10500, "2026-10-01".into())]);
    assert_eq!(first_legs(out.ok(fc1), "2026-12-01"), vec![(1, 1, -11000, "2026-12-02".into()), (2, 2, -10500, "2026-12-02".into())],
               "the moved date follows the new price on every hop");
    out.ok(rerouted);
    let mut nov = first_legs(out.ok(fc2), "2026-11-01");
    nov.sort();
    assert_eq!(nov, vec![(1, 1, -11000, "2026-11-01".into()), (2, 4, -10500, "2026-11-01".into())]);
    let (code, msg) = out.err(expense_stop);
    assert!(code == "bad_chain" && msg.contains("pass through"), "{code}: {msg}");
    let (code, msg) = out.err(extra_stop);
    assert!(code == "bad_chain" && msg.contains("stops"), "{code}: {msg}");
    assert_eq!(out.err(euro_stop).0, "currency_change");
    assert_eq!(out.err(reschedule).0, "slots_detached");
    let sp = out.ok(split);
    assert_eq!(sp["ended"], json!({ "ids": [1, 2], "until_on": "2026-10-31" }));
    assert_eq!(sp["created"]["ids"], json!([3, 4]));
    assert_eq!(sp["created"]["chain_id"], 3);
    assert_eq!(sp["created"]["head_ids"], json!([1, 2]));
    let mut nov = first_legs(out.ok(fc3), "2026-11-01");
    nov.sort();
    assert_eq!(nov, vec![(3, 1, -12000, "2026-11-01".into()), (4, 4, -11500, "2026-11-01".into())]);
    let stakes: Vec<Value> = out.ok(brief)["commitments"]["series"].as_array().unwrap().iter().filter(|c| c["description"] == "stake via Sam").cloned().collect();
    assert_eq!(stakes.len(), 1);
    assert_eq!(stakes[0]["parts"], json!([1, 3]));
    assert_eq!(out.ok(record)["txn_ids"].as_array().unwrap().len(), 2);
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn a_what_if_rule_is_edited_in_place_and_its_cancel_is_not() {
    let mut s = Script::new();
    s.call("scenario.create", json!({ "name": "move out" }));
    s.account("Current", "asset");
    s.account("Rent", "expense");
    let mut dream = plain("Dream rent", 1, 2, 150000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-10-01");
    dream["scenario_id"] = json!(1);
    s.call("series.create", dream);
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.create", json!({ "description": "cancel", "rrule": "FREQ=MONTHLY;BYMONTHDAY=1", "dtstart": "2026-01-01", "scenario_id": 1, "supersedes_id": 2, "postings": [] }));
    let priced = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "amounts": [160000] }));
    let with = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-12-31", "scenarios": [1] }));
    let base = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-12-31" }));
    let from = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2026-11-01", "amounts": [170000] }));
    let cancel = s.call("series.revise", json!({ "id": 3, "as_of": AS_OF, "description": "x" }));
    let txns = s.call("analysis.query", json!({ "sql": "SELECT COUNT(*) FROM txn" }));
    let out = s.run();
    assert_eq!(out.ok(priced)["mode"], "in_place");
    assert!(legs(out.ok(with), 1).iter().any(|l| l.0 == 1 && l.3 == -160000));
    assert!(!legs(out.ok(base), 1).iter().any(|l| l.0 == 1));
    assert_eq!(out.err(from).0, "not_editable");
    assert_eq!(out.err(cancel).0, "not_editable");
    assert_eq!(out.ok(txns)["rows"][0][0], 0, "a what-if is never money that moved");
}

#[test]
fn a_custom_template_can_be_rescheduled_but_not_repriced() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Phone", "expense");
    s.account("Fees", "expense");
    s.call("series.create", json!({
        "description": "Phone", "rrule": "FREQ=MONTHLY;BYMONTHDAY=20", "dtstart": "2026-01-20",
        "postings": [
            { "account_id": 1, "amount_minor": -5000, "role": "primary" },
            { "account_id": 2, "amount_minor": 4000, "role": "balancing" },
            { "account_id": 3, "amount_minor": 1000, "role": "other" },
        ],
    }));
    let amounts = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "amounts": [4000] }));
    let route = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "route": [1, 2] }));
    let weekend = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "weekend_rule": "after" }));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let out = s.run();
    assert_eq!(out.err(amounts).0, "custom_template");
    assert_eq!(out.err(route).0, "custom_template");
    assert_eq!(out.ok(weekend)["mode"], "in_place");
    assert_eq!(rule(out.ok(review), "Phone")["shape"], "custom");
}

#[test]
fn a_route_edit_keeps_each_hops_direction() {
    // A template written the other way round: the primary leg is the account money ARRIVES in.
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Salary", "income");
    s.account("Savings", "asset");
    s.call("series.create", json!({
        "description": "Pay", "rrule": "FREQ=MONTHLY;BYMONTHDAY=28", "dtstart": "2026-01-28",
        "postings": [
            { "account_id": 1, "amount_minor": 250000, "role": "primary" },
            { "account_id": 2, "amount_minor": -250000, "role": "balancing" },
        ],
    }));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let moved = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "route": [2, 3], "amounts": [260000] }));
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-09-30" }));
    let out = s.run();
    let r = rule(out.ok(review), "Pay");
    let route: Vec<i64> = r["current"]["route"].as_array().unwrap().iter().map(|a| a["account_id"].as_i64().unwrap()).collect();
    assert_eq!(route, vec![2, 1], "money comes from Salary into Current");
    assert_eq!(r["direction"], "in");
    out.ok(moved);
    let pay: Vec<(i64, i64)> = out.ok(fc)["occurrences"].as_array().unwrap().iter()
        .filter(|o| o["kind"] == "series").map(|o| (o["account_id"].as_i64().unwrap(), o["amount_minor"].as_i64().unwrap())).collect();
    assert!(pay.contains(&(3, 260000)) && pay.contains(&(2, -260000)), "still income, now into Savings: {pay:?}");
}

#[test]
fn a_stored_rule_that_does_not_expand_is_reported_and_can_be_fixed() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.account("Gym", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=NOPE", "2026-01-01"));
    s.call("series.create", plain("Gym", 1, 3, 3000, "FREQ=MONTHLY;BYMONTHDAY=10", "2026-01-10"));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let fixed = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "rrule": "FREQ=MONTHLY;BYMONTHDAY=1" }));
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2026-10-31" }));
    let out = s.run();
    let v = out.ok(review);
    assert!(rule(v, "Rent")["rule_error"].is_string(), "{}", rule(v, "Rent"));
    assert_eq!(rule(v, "Gym")["next"][0]["occurrence_on"], "2026-10-10", "one broken rule hides nothing else");
    let f = out.ok(fixed);
    assert!(f["impact"]["before_error"].is_string());
    assert!(f["impact"]["after_error"].is_null());
    out.ok(fc);
}

#[test]
fn a_claim_cut_off_by_an_end_date_still_blocks_a_schedule_change() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-12-01", "occurred_on": "2026-09-10" }));
    s.call("series.end", json!({ "id": 1, "until_on": "2026-11-30" }));
    let edit = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "until_on": null, "rrule": "FREQ=MONTHLY;BYMONTHDAY=5" }));
    let out = s.run();
    assert_eq!(out.err(edit).0, "slots_detached", "December is recorded, even though the end date cut it off");
}

#[test]
fn undoing_a_change_puts_the_rule_back() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    let window = json!({ "as_of": AS_OF, "horizon": "2027-06-30" });
    let p0 = s.call("forecast.project", window.clone());
    let change = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    s.call("series.override", json!({ "series_id": 2, "occurrence_on": "2027-02-01", "action": "skip" }));
    let dry = s.call("series.undo_change", json!({ "id": 2, "as_of": AS_OF, "dry_run": true }));
    let older = s.call("series.undo_change", json!({ "id": 1, "as_of": AS_OF }));
    let undo = s.call("series.undo_change", json!({ "id": 2, "as_of": AS_OF }));
    let list = s.call("series.list", json!({}));
    let p1 = s.call("forecast.project", window);
    let nothing = s.call("series.undo_change", json!({ "id": 1, "as_of": AS_OF }));
    let again = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    let out_first = s.run();
    let new_id = out_first.ok(again)["created"]["ids"][0].as_i64().unwrap();

    s.call("series.record", json!({ "series_id": new_id, "occurrence_on": "2027-01-01" }));
    let blocked = s.call("series.undo_change", json!({ "id": new_id, "as_of": AS_OF }));
    let list2 = s.call("series.list", json!({}));
    let check = s.call("db.check", json!({}));
    let out = s.run();

    assert_eq!(out.ok(change)["created"]["ids"], json!([2]));
    let d = out.ok(dry);
    assert_eq!(d["removed"], json!([2]));
    assert_eq!(d["restored"], json!({ "ids": [1], "until_on": null }));
    assert_eq!(d["carried_back"][0]["occurrence_on"], "2027-02-01");
    assert_eq!(out.err(older).0, "not_latest");
    out.ok(undo);
    let rows = out.ok(list).as_array().unwrap().clone();
    assert_eq!((rows.len(), rows[0]["until_on"].clone()), (1, Value::Null));
    let without_feb: Vec<Value> = out.ok(p0)["occurrences"].as_array().unwrap().iter().filter(|o| o["occurrence_on"] != "2027-02-01").cloned().collect();
    assert_eq!(out.ok(p1)["occurrences"], Value::Array(without_feb), "the rule is back as it was, with the skip it gained");
    assert_eq!(out.err(nothing).0, "not_a_change");
    assert_eq!(out.err(blocked).0, "change_has_payments");
    assert_eq!(out.ok(list2).as_array().unwrap().len(), 2);
    assert_eq!(out.ok(check)["ok"], true);
}

#[test]
fn ending_and_renaming_a_changed_rule_act_on_the_whole_rule() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.record", json!({ "series_id": 1, "occurrence_on": "2026-09-01" }));
    s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    let early = s.call("series.end", json!({ "id": 1, "until_on": "2026-11-30" }));
    let end = s.call("series.end", json!({ "id": 1, "until_on": "2027-06-01" }));
    let list1 = s.call("series.list", json!({}));
    let unbound = s.call("series.end", json!({ "id": 2 }));
    let rename = s.call("series.rename", json!({ "id": 2, "description": "Flat rent" }));
    let txn = s.call("txn.get", json!({ "id": 1 }));
    let stale = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "amounts": [99000] }));
    let list2 = s.call("series.list", json!({}));
    let out = s.run();
    let (code, msg) = out.err(early);
    assert!(code == "overlaps_part" && msg.contains("1/1/2027"), "{code}: {msg}");
    assert_eq!(out.ok(end)["applied_to"], json!([2]));
    let until: Vec<Value> = out.ok(list1).as_array().unwrap().iter().map(|r| r["until_on"].clone()).collect();
    assert_eq!(until, vec![json!("2026-12-31"), json!("2027-06-01")]);
    assert_eq!(out.ok(unbound)["until_on"], Value::Null);
    assert_eq!(out.ok(rename)["applied_to"], json!([1, 2]));
    assert_eq!(out.ok(txn)["description"], "Rent", "a rename is not retrospective");
    assert_eq!(out.err(stale).0, "not_latest");
    let rows = out.ok(list2).as_array().unwrap().clone();
    assert_eq!((rows[0]["description"].as_str(), rows[1]["description"].as_str()), (Some("Flat rent"), Some("Flat rent")));
    assert_eq!((rows[0]["latest"].as_bool(), rows[0]["lineage_id"].as_i64(), rows[0]["head_id"].clone()), (Some(false), Some(1), Value::Null));
    assert_eq!((rows[1]["latest"].as_bool(), rows[1]["head_id"].as_i64()), (Some(true), Some(1)));
    assert_eq!((rows[1]["from_account_id"].as_i64(), rows[1]["to_account_id"].as_i64()), (Some(1), Some(2)));
}

#[test]
fn an_unchanged_save_writes_nothing() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    let before = s.call("series.list", json!({}));
    let same = json!({ "id": 1, "as_of": AS_OF, "description": "Rent", "rrule": "FREQ=MONTHLY;BYMONTHDAY=1", "dtstart": "2026-01-01",
                        "weekend_rule": "none", "until_on": null, "route": [1, 2], "amounts": [90000] });
    let whole = s.call("series.revise", same.clone());
    let mut from = same;
    from["mode"] = json!("from");
    from["from_on"] = json!("2026-11-01");
    let from = s.call("series.revise", from);
    let after = s.call("series.list", json!({}));
    let out = s.run();
    for id in [whole, from] {
        assert_eq!((out.ok(id)["mode"].as_str(), out.ok(id)["written"].as_bool()), (Some("unchanged"), Some(false)));
    }
    assert_eq!(out.ok(before), out.ok(after));
}

#[test]
fn a_preview_can_start_from_a_date_and_keep_the_rhythm() {
    let out = run(&[r#"{"id":1,"method":"series.preview","params":{"rrule":"FREQ=WEEKLY;INTERVAL=2;BYDAY=FR","dtstart":"2026-08-07","from_on":"2027-01-01","count":2}}"#]);
    let dates: Vec<&str> = out[0]["result"]["dates"].as_array().unwrap().iter().map(|d| d["occurrence_on"].as_str().unwrap()).collect();
    assert_eq!(dates, vec!["2027-01-08", "2027-01-22"]);
}

#[test]
fn a_new_schedule_for_a_changed_rule_starts_the_day_after_the_earlier_terms_end() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=15", "2026-01-15"));
    s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    let fifth = s.call("series.revise", json!({ "id": 2, "as_of": AS_OF, "mode": "whole", "rrule": "FREQ=MONTHLY;BYMONTHDAY=5" }));
    let list = s.call("series.list", json!({}));
    let fc = s.call("forecast.project", json!({ "as_of": AS_OF, "horizon": "2027-03-31" }));
    let out = s.run();
    assert_eq!(out.ok(fifth)["mode"], "in_place");
    let rows = out.ok(list).as_array().unwrap().clone();
    assert_eq!((rows[0]["until_on"].as_str(), rows[1]["dtstart"].as_str()), (Some("2026-12-31"), Some("2027-01-05")),
               "not the 15/1/2027 the old schedule started on, which would lose 5/1/2027 between the parts");
    let dates: Vec<(i64, String)> = legs(out.ok(fc), 1).into_iter().map(|l| (l.0, l.1)).collect();
    assert_eq!(dates, vec![(1, "2026-09-15".into()), (1, "2026-10-15".into()), (1, "2026-11-15".into()), (1, "2026-12-15".into()),
                           (2, "2027-01-05".into()), (2, "2027-02-05".into()), (2, "2027-03-05".into())]);
}

#[test]
fn a_flag_that_is_not_true_or_false_is_refused_rather_than_read_as_false() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    let quoted = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "amounts": [95000], "dry_run": "true" }));
    let number = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "amounts": [95000], "acknowledge_due": 1 }));
    s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    let undo = s.call("series.undo_change", json!({ "id": 2, "as_of": AS_OF, "dry_run": "yes" }));
    let list = s.call("series.list", json!({}));
    let out = s.run();
    let (code, msg) = out.err(quoted);
    assert!(code == "bad_params" && msg.contains("dry_run"), "{code}: {msg}");
    assert_eq!(out.err(number).0, "bad_params");
    assert_eq!(out.err(undo).0, "bad_params");
    assert_eq!(out.ok(list).as_array().unwrap().len(), 2, "the change was not undone");
}

#[test]
fn a_change_from_a_date_that_moves_no_payment_applies_to_the_current_terms_whatever_the_date() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.account("Gym", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2027-01-01", "amounts": [95000] }));
    let mut gym = plain("Gym", 1, 3, 3000, "FREQ=MONTHLY;BYMONTHDAY=10", "2026-01-10");
    gym["until_on"] = json!("2026-06-10");
    s.call("series.create", gym);
    let rename = s.call("series.revise", json!({ "id": 2, "as_of": AS_OF, "mode": "from", "from_on": "2026-11-01", "description": "Flat rent" }));
    let reprice = s.call("series.revise", json!({ "id": 2, "as_of": AS_OF, "mode": "from", "from_on": "2026-11-01", "amounts": [99000] }));
    let weekend = s.call("series.revise", json!({ "id": 3, "as_of": AS_OF, "mode": "from", "from_on": AS_OF, "weekend_rule": "before" }));
    let reprice_ended = s.call("series.revise", json!({ "id": 3, "as_of": AS_OF, "mode": "from", "from_on": AS_OF, "amounts": [3500] }));
    let out = s.run();
    assert_eq!(out.ok(rename)["mode"], "in_place");
    let (code, msg) = out.err(reprice);
    assert!(code == "overlaps_part" && msg.contains("change it from 1/1/2027 or later"), "{code}: {msg}");
    assert_eq!(out.ok(weekend)["mode"], "in_place", "an ended rule's weekend rule is not a change from a date");
    assert_eq!(out.err(reprice_ended).0, "already_ended");
}

#[test]
fn the_review_counts_a_payment_as_due_by_when_its_money_moves() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Gym", "expense");
    let mut gym = plain("Gym", 1, 2, 3000, "FREQ=MONTHLY;BYMONTHDAY=12", "2026-09-12");
    gym["weekend_rule"] = json!("after");
    s.call("series.create", gym);
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let out = s.run();
    let g = rule(out.ok(review), "Gym");
    assert_eq!(g["due"], json!([]), "12/9/2026 is a Saturday paid on Monday 14/9, which is today: {g}");
    assert_eq!((g["next"][0]["occurrence_on"].as_str(), g["next"][0]["value_on"].as_str()), (Some("2026-09-12"), Some("2026-09-14")));
}

#[test]
fn an_adjustment_left_after_a_rule_ends_is_not_listed_as_ahead() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2026-11-01", "amount_minor": -91000 }));
    s.call("series.override", json!({ "series_id": 1, "occurrence_on": "2027-02-01", "action": "skip" }));
    let end = s.call("series.end", json!({ "id": 1, "until_on": "2026-12-31" }));
    let review = s.call("series.review", json!({ "as_of": AS_OF }));
    let out = s.run();
    out.ok(end);
    let on: Vec<&str> = rule(out.ok(review), "Rent")["overrides"].as_array().unwrap().iter()
        .map(|o| o["occurrence_on"].as_str().unwrap()).collect();
    assert_eq!(on, vec!["2026-11-01"]);
}

#[test]
fn the_brief_names_the_account_a_changed_rule_pays_from_now() {
    let mut s = Script::new();
    s.account("Current", "asset");
    s.account("Rent", "expense");
    s.account("Joint", "asset");
    s.call("series.create", plain("Rent", 1, 2, 90000, "FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01"));
    let moved = s.call("series.revise", json!({ "id": 1, "as_of": AS_OF, "mode": "from", "from_on": "2026-11-01",
                                                "route": [3, 2] }));
    let early = s.call("series.end", json!({ "id": 2, "until_on": "2026-10-15" }));
    let before_start = s.call("series.create", plain("Gym", 1, 2, 3000, "FREQ=MONTHLY;BYMONTHDAY=10", "2026-01-10"));
    let gym_end = s.call("series.end", json!({ "id": 3, "until_on": "2025-12-31" }));
    let brief = s.call("analysis.brief", json!({ "as_of": AS_OF }));
    let out = s.run();
    assert_eq!(out.ok(moved)["mode"], "split");
    let (_, msg) = out.err(early);
    assert!(msg.contains("changes on 1/11/2026"), "a change still ahead is spoken of as ahead: {msg}");
    out.ok(before_start);
    let (_, msg) = out.err(gym_end);
    assert!(msg.contains("31/12/2025") && msg.contains("10/1/2026"), "{msg}");
    let rows = out.ok(brief)["commitments"]["series"].as_array().unwrap().clone();
    let rent = rows.iter().find(|r| r["description"] == "Rent").unwrap();
    assert_eq!(rent["account"], "Joint", "{rent}");
}
