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
