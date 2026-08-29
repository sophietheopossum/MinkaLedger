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
