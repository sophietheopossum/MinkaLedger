//! CSV import: stage, review, commit, revert.
//!
//! NOTHING IS WRITTEN TO THE LEDGER BY IMPORTING. A file lands in `import_row` as a staged batch
//! you can look at, correct and throw away. Only `commit` creates transactions, and `revert` undoes
//! exactly what a batch created. That is GnuCash's review-before-commit shape, and it is what makes
//! importing a bank export safe to do casually: a bad mapping costs you a `revert`, not an evening
//! picking 300 wrong transactions out of a ledger.
//!
//! DEDUPLICATION is by a fingerprint that is DELIBERATELY HUMAN-READABLE -- `date|amount|payee` --
//! rather than a hash. When two rows collide you can see why, and when a legitimate pair of
//! identical payments on the same day collides you can tell that is what happened. A hash would
//! make both cases opaque and identical.
//!
//! THE FAR SIDE. A bank CSV tells you one side of each transaction. The other side is a guess, so
//! every row lands against a single `Expenses:Unclassified` account unless an `import_rule` says
//! otherwise. That keeps the book balanced and honest -- the unclassified balance is a visible
//! to-do list rather than a silent misfiling.

use rusqlite::Connection;

use crate::money::{parse_minor, Minor};

#[derive(Debug)]
pub enum ImportError {
    Sql(rusqlite::Error),
    Csv(String),
    NoProfile(i64),
    NoBatch(i64),
    BadMapping(String),
    AlreadyCommitted(i64),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Sql(e) => write!(f, "{e}"),
            ImportError::Csv(m) => write!(f, "csv: {m}"),
            ImportError::NoProfile(id) => write!(f, "no such import profile: {id}"),
            ImportError::NoBatch(id) => write!(f, "no such import batch: {id}"),
            ImportError::BadMapping(m) => write!(f, "bad mapping: {m}"),
            ImportError::AlreadyCommitted(id) => write!(f, "batch {id} is already committed"),
        }
    }
}

impl From<rusqlite::Error> for ImportError {
    fn from(e: rusqlite::Error) -> Self {
        ImportError::Sql(e)
    }
}

/// Which CSV column feeds which field. Stored as JSON on the profile so a new bank needs a row,
/// not a code change.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct Mapping {
    pub date: String,
    /// A single signed amount column...
    #[serde(default)]
    pub amount: Option<String>,
    /// ...or separate money-in / money-out columns, which several UK banks use instead.
    #[serde(default)]
    pub money_in: Option<String>,
    #[serde(default)]
    pub money_out: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub payee: Option<String>,
    #[serde(default)]
    pub bank_category: Option<String>,
    #[serde(default)]
    pub txn_type: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub balance: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
}

/// Normalise a description for fingerprinting: collapse whitespace, uppercase, and drop the
/// trailing reference numbers banks append, which differ between an export and a re-export.
fn normalise(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .collect::<String>()
        .trim()
        .chars()
        .take(48)
        .collect()
}

/// A stable, inspectable row identity. Not a hash -- see the module note.
pub fn fingerprint(occurred_on: &str, amount_minor: Minor, description: &str) -> String {
    format!("{occurred_on}|{amount_minor}|{}", normalise(description))
}

/// Parse a date in the profile's format. Handles the three shapes UK exports actually use.
fn parse_date(text: &str, format: &str) -> Option<String> {
    let t = text.trim();
    let d = chrono::NaiveDate::parse_from_str(t, format)
        // ISO is worth trying unconditionally: several exports claim one format and emit ISO.
        .or_else(|_| chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d"))
        .or_else(|_| chrono::NaiveDate::parse_from_str(t, "%d/%m/%Y"))
        .ok()?;
    Some(d.to_string())
}

fn col<'a>(rec: &'a csv::StringRecord, headers: &csv::StringRecord, name: &str) -> Option<&'a str> {
    let idx = headers.iter().position(|h| h.trim().eq_ignore_ascii_case(name.trim()))?;
    rec.get(idx)
}

pub struct StageReport {
    pub batch_id: i64,
    pub rows: usize,
    pub new_rows: usize,
    pub duplicates: usize,
    pub errors: usize,
}

/// Read `csv_text` through `profile_id` into a new staged batch. Writes nothing to the ledger.
pub fn stage(
    conn: &mut Connection,
    profile_id: i64,
    source_name: &str,
    csv_text: &str,
) -> Result<StageReport, ImportError> {
    let (mapping_json, date_format, account_id, default_currency, delimiter): (
        String, String, Option<i64>, String, String,
    ) = conn
        .query_row(
            "SELECT mapping_json, date_format, account_id, default_currency, delimiter
               FROM import_profile WHERE id = ?1",
            [profile_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(|_| ImportError::NoProfile(profile_id))?;
    let mapping: Mapping = serde_json::from_str(&mapping_json)
        .map_err(|e| ImportError::BadMapping(e.to_string()))?;
    let minor_digits: u32 = conn
        .query_row("SELECT minor_digits FROM currency WHERE code=?1", [&default_currency], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(2) as u32;

    let delim = delimiter.as_bytes().first().copied().unwrap_or(b',');
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .flexible(true)
        .from_reader(csv_text.as_bytes());
    let headers = rdr.headers().map_err(|e| ImportError::Csv(e.to_string()))?.clone();

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO import_batch(profile_id, source_name, file_fingerprint, imported_at, state)
         VALUES(?1, ?2, ?3, datetime('now'), 'staged')",
        rusqlite::params![profile_id, source_name, fingerprint("file", csv_text.len() as i64, source_name)],
    )?;
    let batch_id = tx.last_insert_rowid();

    let mut report = StageReport { batch_id, rows: 0, new_rows: 0, duplicates: 0, errors: 0 };
    let mut first_on: Option<String> = None;
    let mut last_on: Option<String> = None;

    for (i, rec) in rdr.records().enumerate() {
        let line_no = i as i64 + 1;
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                tx.execute(
                    "INSERT INTO import_row(batch_id, line_no, raw_json, state, error)
                     VALUES(?1,?2,'{}','error',?3)",
                    rusqlite::params![batch_id, line_no, e.to_string()],
                )?;
                report.errors += 1;
                report.rows += 1;
                continue;
            }
        };
        report.rows += 1;
        let raw: Vec<&str> = rec.iter().collect();
        let raw_json = serde_json::to_string(&raw).unwrap_or_else(|_| "[]".into());

        let occurred_on = col(&rec, &headers, &mapping.date).and_then(|t| parse_date(t, &date_format));
        // Either one signed column, or separate in/out columns.
        let amount = match (&mapping.amount, &mapping.money_in, &mapping.money_out) {
            (Some(a), _, _) => col(&rec, &headers, a)
                .filter(|s| !s.trim().is_empty())
                .and_then(|t| parse_minor(t, minor_digits).ok()),
            (None, in_col, out_col) => {
                let inn = in_col
                    .as_ref()
                    .and_then(|c| col(&rec, &headers, c))
                    .filter(|s| !s.trim().is_empty())
                    .and_then(|t| parse_minor(t, minor_digits).ok());
                let out = out_col
                    .as_ref()
                    .and_then(|c| col(&rec, &headers, c))
                    .filter(|s| !s.trim().is_empty())
                    .and_then(|t| parse_minor(t, minor_digits).ok());
                match (inn, out) {
                    (Some(v), _) if v != 0 => Some(v),
                    (_, Some(v)) if v != 0 => Some(-v.abs()),
                    _ => None,
                }
            }
        };
        let description = mapping
            .description
            .as_ref()
            .and_then(|c| col(&rec, &headers, c))
            .unwrap_or("")
            .trim()
            .to_string();
        let payee = mapping.payee.as_ref().and_then(|c| col(&rec, &headers, c)).map(str::to_string);
        let bank_category =
            mapping.bank_category.as_ref().and_then(|c| col(&rec, &headers, c)).map(str::to_string);
        let txn_type =
            mapping.txn_type.as_ref().and_then(|c| col(&rec, &headers, c)).map(str::to_string);
        let currency = mapping
            .currency
            .as_ref()
            .and_then(|c| col(&rec, &headers, c))
            .map(str::to_string)
            .unwrap_or_else(|| default_currency.clone());
        let external_id =
            mapping.external_id.as_ref().and_then(|c| col(&rec, &headers, c)).map(str::to_string);

        let (state, err) = match (&occurred_on, amount) {
            (Some(_), Some(_)) => ("pending", None),
            (None, _) => ("error", Some("unparseable date".to_string())),
            (_, None) => ("error", Some("unparseable amount".to_string())),
        };
        if err.is_some() {
            report.errors += 1;
        }

        let fp = match (&occurred_on, amount) {
            (Some(d), Some(a)) => Some(fingerprint(d, a, &description)),
            _ => None,
        };
        // Already imported? Dedup is per ACCOUNT, so the same payment appearing in two different
        // accounts' exports is two real rows, not a duplicate.
        let mut state = state.to_string();
        let mut dup_of: Option<i64> = None;
        if let (Some(fp), Some(acct)) = (&fp, account_id) {
            if let Ok(existing) = tx.query_row(
                "SELECT txn_id FROM txn_import_key WHERE account_id = ?1 AND fingerprint = ?2",
                rusqlite::params![acct, fp],
                |r| r.get::<_, i64>(0),
            ) {
                state = "duplicate".into();
                dup_of = Some(existing);
                report.duplicates += 1;
            }
        }
        if state == "pending" {
            state = "new".into();
            report.new_rows += 1;
        }

        if let Some(d) = &occurred_on {
            if first_on.as_ref().is_none_or(|f| d < f) {
                first_on = Some(d.clone());
            }
            if last_on.as_ref().is_none_or(|l| d > l) {
                last_on = Some(d.clone());
            }
        }

        tx.execute(
            "INSERT INTO import_row(batch_id, line_no, raw_json, account_id, occurred_on,
                description, payee, bank_category, txn_type, amount_minor, currency,
                external_id, fingerprint, state, error, dup_of_txn_id, accepted)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            rusqlite::params![
                batch_id, line_no, raw_json, account_id, occurred_on, description, payee,
                bank_category, txn_type, amount, currency, external_id, fp, state, err, dup_of,
                i64::from(state == "new")
            ],
        )?;
    }

    tx.execute(
        "UPDATE import_batch SET row_count=?2, first_row_on=?3, last_row_on=?4 WHERE id=?1",
        rusqlite::params![batch_id, report.rows as i64, first_on, last_on],
    )?;
    tx.commit()?;
    Ok(report)
}

/// Apply the enabled categorisation rules to a staged batch, setting the far side of each row.
/// Highest priority first; the first match wins and records which rule did it.
pub fn categorise(conn: &mut Connection, batch_id: i64) -> Result<usize, ImportError> {
    let rules: Vec<(i64, String, String, String, i64, Option<i64>)> = conn
        .prepare(
            "SELECT id, field, op, pattern, sign, set_far_account_id FROM import_rule
              WHERE enabled = 1 ORDER BY priority DESC, id",
        )?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<Result<_, _>>()?;

    let rows: Vec<(i64, String, Option<String>, Option<String>, Option<String>, Minor)> = conn
        .prepare(
            "SELECT id, description, payee, bank_category, txn_type, COALESCE(amount_minor,0)
               FROM import_row WHERE batch_id = ?1 AND state IN ('new','pending')",
        )?
        .query_map([batch_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<Result<_, _>>()?;

    let tx = conn.transaction()?;
    let mut hits = 0;
    for (row_id, desc, payee, cat, ttype, amount) in rows {
        for (rid, field, op, pattern, sign, far) in &rules {
            // A rule may be restricted to money in or money out -- "TESCO" is a shop when money
            // leaves and a refund when it arrives.
            if (*sign > 0 && amount <= 0) || (*sign < 0 && amount >= 0) {
                continue;
            }
            let hay = match field.as_str() {
                "payee" => payee.clone().unwrap_or_default(),
                "bank_category" => cat.clone().unwrap_or_default(),
                "txn_type" => ttype.clone().unwrap_or_default(),
                _ => desc.clone(),
            }
            .to_uppercase();
            let needle = pattern.to_uppercase();
            let matched = match op.as_str() {
                "equals" => hay == needle,
                "starts_with" => hay.starts_with(&needle),
                // `regex` is accepted by the schema but treated as `contains` here: pulling in a
                // regex engine for a personal importer is not worth the dependency, and silently
                // doing something DIFFERENT from what was asked would be worse than this note.
                _ => hay.contains(&needle),
            };
            if matched {
                tx.execute(
                    "UPDATE import_row SET far_account_id = ?2, rule_id = ?3 WHERE id = ?1",
                    rusqlite::params![row_id, far, rid],
                )?;
                tx.execute(
                    "UPDATE import_rule SET hit_count = hit_count + 1, last_hit_on = date('now')
                      WHERE id = ?1",
                    [rid],
                )?;
                hits += 1;
                break;
            }
        }
    }
    tx.commit()?;
    Ok(hits)
}

/// Turn accepted rows into real transactions. Rows with no far account go to `Expenses:Unclassified`.
pub fn commit(conn: &mut Connection, batch_id: i64) -> Result<usize, ImportError> {
    let state: String = conn
        .query_row("SELECT state FROM import_batch WHERE id=?1", [batch_id], |r| r.get(0))
        .map_err(|_| ImportError::NoBatch(batch_id))?;
    if state == "committed" {
        return Err(ImportError::AlreadyCommitted(batch_id));
    }

    let unclassified = unclassified_account(conn)?;
    let rows: Vec<(i64, i64, String, String, Option<String>, Minor, String, Option<i64>, Option<String>)> =
        conn.prepare(
            "SELECT id, account_id, occurred_on, description, payee, amount_minor, currency,
                    far_account_id, fingerprint
               FROM import_row
              WHERE batch_id = ?1 AND accepted = 1 AND state IN ('new','matched')
                AND account_id IS NOT NULL AND occurred_on IS NOT NULL AND amount_minor IS NOT NULL
              ORDER BY line_no",
        )?
        .query_map([batch_id], |r| {
            Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                r.get(7)?, r.get(8)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    let tx = conn.transaction()?;
    let mut n = 0;
    for (row_id, account_id, on, desc, payee, amount, currency, far, fp) in rows {
        let far = far.unwrap_or(unclassified);
        tx.execute(
            "INSERT INTO txn(occurred_on, description, payee, source) VALUES(?1,?2,?3,'import')",
            rusqlite::params![on, desc, payee],
        )?;
        let txn_id = tx.last_insert_rowid();
        let far_currency: String =
            tx.query_row("SELECT currency FROM account WHERE id=?1", [far], |r| r.get(0))?;
        {
            let mut st = tx.prepare(
                "INSERT INTO posting(txn_id, account_id, currency, amount_minor) VALUES(?1,?2,?3,?4)",
            )?;
            st.execute(rusqlite::params![txn_id, account_id, currency, amount])?;
            st.execute(rusqlite::params![txn_id, far, far_currency, -amount])?;
        }
        if let Some(fp) = fp {
            tx.execute(
                "INSERT OR IGNORE INTO txn_import_key(txn_id, account_id, fingerprint, batch_id, line_no)
                 VALUES(?1,?2,?3,?4,(SELECT line_no FROM import_row WHERE id=?5))",
                rusqlite::params![txn_id, account_id, fp, batch_id, row_id],
            )?;
        }
        tx.execute(
            "UPDATE import_row SET state='committed', txn_id=?2 WHERE id=?1",
            rusqlite::params![row_id, txn_id],
        )?;
        n += 1;
    }
    tx.execute(
        "UPDATE import_batch SET state='committed', committed_at=datetime('now') WHERE id=?1",
        [batch_id],
    )?;
    tx.commit()?;
    Ok(n)
}

/// Undo exactly what a batch created. The ledger returns to its previous state.
pub fn revert(conn: &mut Connection, batch_id: i64) -> Result<usize, ImportError> {
    let ids: Vec<i64> = conn
        .prepare("SELECT txn_id FROM import_row WHERE batch_id=?1 AND txn_id IS NOT NULL")?
        .query_map([batch_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let tx = conn.transaction()?;
    for id in &ids {
        // postings and txn_import_key both cascade from txn
        tx.execute("DELETE FROM txn WHERE id = ?1", [id])?;
    }
    tx.execute(
        "UPDATE import_row SET state='new', txn_id=NULL WHERE batch_id=?1 AND state='committed'",
        [batch_id],
    )?;
    tx.execute("UPDATE import_batch SET state='reverted' WHERE id=?1", [batch_id])?;
    tx.commit()?;
    Ok(ids.len())
}

/// The bucket an uncategorised row lands in, created on first use.
///
/// Resolved by crate::roles rather than by the literal name: `account.rename` can retitle this
/// account, and a name lookup would then quietly create a SECOND one -- splitting old uncategorised
/// spend from new across two accounts no report joins.
fn unclassified_account(conn: &Connection) -> Result<i64, ImportError> {
    Ok(crate::roles::unclassified(conn)?)
}

pub fn rows(conn: &Connection, batch_id: i64) -> Result<Vec<serde_json::Value>, ImportError> {
    let mut st = conn.prepare(
        "SELECT r.id, r.line_no, r.occurred_on, r.description, r.amount_minor, r.state,
                r.accepted, r.far_account_id, a.name, r.error
           FROM import_row r LEFT JOIN account a ON a.id = r.far_account_id
          WHERE r.batch_id = ?1 ORDER BY r.line_no",
    )?;
    let out = st
        .query_map([batch_id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "line_no": r.get::<_, i64>(1)?,
                "occurred_on": r.get::<_, Option<String>>(2)?,
                "description": r.get::<_, String>(3)?,
                "amount_minor": r.get::<_, Option<i64>>(4)?,
                "state": r.get::<_, String>(5)?,
                "accepted": r.get::<_, i64>(6)? == 1,
                "far_account_id": r.get::<_, Option<i64>>(7)?,
                "far_account": r.get::<_, Option<String>>(8)?,
                "error": r.get::<_, Option<String>>(9)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONZO: &str = "\
Date,Description,Amount,Category
01/08/2026,TESCO STORES 3345,-42.15,Groceries
02/08/2026,SALARY ACME LTD,2500.00,Income
03/08/2026,NETFLIX.COM,-10.99,Entertainment
";

    fn book() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql")).unwrap();
        conn.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES
               (1,'Current','asset','GBP'),
               (2,'Groceries','expense','GBP'),
               (3,'Salary','income','GBP');",
        )
        .unwrap();
        let mapping = serde_json::to_string(&Mapping {
            date: "Date".into(),
            amount: Some("Amount".into()),
            description: Some("Description".into()),
            bank_category: Some("Category".into()),
            ..Default::default()
        })
        .unwrap();
        conn.execute(
            "INSERT INTO import_profile(id, name, date_format, mapping_json, account_id, default_currency)
             VALUES(1,'Monzo','%d/%m/%Y',?1,1,'GBP')",
            [mapping],
        )
        .unwrap();
        conn
    }

    #[test]
    fn staging_parses_rows_without_touching_the_ledger() {
        let mut c = book();
        let rep = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        assert_eq!(rep.rows, 3);
        assert_eq!(rep.new_rows, 3);
        assert_eq!(rep.errors, 0);
        // nothing in the ledger yet -- that is the whole point
        let n: i64 = c.query_row("SELECT COUNT(*) FROM txn", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);

        let rows = rows(&c, rep.batch_id).unwrap();
        assert_eq!(rows[0]["occurred_on"], "2026-08-01");
        assert_eq!(rows[0]["amount_minor"], -4_215);
        assert_eq!(rows[1]["amount_minor"], 250_000);
    }

    #[test]
    fn committing_creates_balanced_transactions() {
        let mut c = book();
        let rep = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        let n = commit(&mut c, rep.batch_id).unwrap();
        assert_eq!(n, 3);
        let report = crate::db::integrity(&c).unwrap();
        assert_eq!(report["ok"], serde_json::json!(true), "{report}");
        // the far side landed in Unclassified, which is a visible to-do rather than a silent guess
        let unclassified: i64 = c
            .query_row(
                "SELECT COALESCE(SUM(p.amount_minor),0) FROM posting p
                   JOIN account a ON a.id=p.account_id WHERE a.name='Expenses:Unclassified'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unclassified, 4_215 - 250_000 + 1_099);
    }

    #[test]
    fn re_importing_the_same_file_adds_nothing() {
        let mut c = book();
        let first = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        commit(&mut c, first.batch_id).unwrap();

        let second = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        assert_eq!(second.duplicates, 3, "every row is already in the book");
        assert_eq!(second.new_rows, 0);
        let n = commit(&mut c, second.batch_id).unwrap();
        assert_eq!(n, 0, "a duplicate row is not accepted, so nothing is written");
        let total: i64 = c.query_row("SELECT COUNT(*) FROM txn", [], |r| r.get(0)).unwrap();
        assert_eq!(total, 3, "still just the three original transactions");
    }

    #[test]
    fn an_overlapping_window_imports_only_the_new_rows() {
        let mut c = book();
        let first = stage(&mut c, 1, "a.csv", MONZO).unwrap();
        commit(&mut c, first.batch_id).unwrap();

        let overlapping = format!("{MONZO}04/08/2026,COFFEE,-3.50,Eating out\n");
        let second = stage(&mut c, 1, "b.csv", &overlapping).unwrap();
        assert_eq!(second.duplicates, 3);
        assert_eq!(second.new_rows, 1);
        commit(&mut c, second.batch_id).unwrap();
        let total: i64 = c.query_row("SELECT COUNT(*) FROM txn", [], |r| r.get(0)).unwrap();
        assert_eq!(total, 4);
    }

    #[test]
    fn revert_restores_the_ledger_exactly() {
        let mut c = book();
        let rep = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        commit(&mut c, rep.batch_id).unwrap();
        assert_eq!(revert(&mut c, rep.batch_id).unwrap(), 3);

        let txns: i64 = c.query_row("SELECT COUNT(*) FROM txn", [], |r| r.get(0)).unwrap();
        let posts: i64 = c.query_row("SELECT COUNT(*) FROM posting", [], |r| r.get(0)).unwrap();
        let keys: i64 = c.query_row("SELECT COUNT(*) FROM txn_import_key", [], |r| r.get(0)).unwrap();
        assert_eq!((txns, posts, keys), (0, 0, 0), "reverting leaves no trace in the ledger");
        // and because the import keys went too, the file can be imported again
        let again = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        assert_eq!(again.new_rows, 3);
    }

    #[test]
    fn rules_set_the_far_side_and_respect_sign() {
        let mut c = book();
        c.execute_batch(
            "INSERT INTO import_rule(name,priority,field,op,pattern,sign,set_far_account_id)
               VALUES('Tesco',100,'description','contains','TESCO',-1,2);
             INSERT INTO import_rule(name,priority,field,op,pattern,sign,set_far_account_id)
               VALUES('Salary',100,'description','contains','SALARY',1,3);",
        )
        .unwrap();
        let rep = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        let hits = categorise(&mut c, rep.batch_id).unwrap();
        assert_eq!(hits, 2, "Tesco and Salary matched; Netflix has no rule");

        let rows = rows(&c, rep.batch_id).unwrap();
        assert_eq!(rows[0]["far_account"], "Groceries");
        assert_eq!(rows[1]["far_account"], "Salary");
        assert!(rows[2]["far_account"].is_null(), "unmatched stays unclassified");
    }

    #[test]
    fn a_sign_restricted_rule_ignores_the_wrong_direction() {
        let mut c = book();
        // a rule for INCOMING Tesco (a refund) must not catch the outgoing purchase
        c.execute_batch(
            "INSERT INTO import_rule(name,priority,field,op,pattern,sign,set_far_account_id)
               VALUES('Tesco refund',100,'description','contains','TESCO',1,3);",
        )
        .unwrap();
        let rep = stage(&mut c, 1, "monzo.csv", MONZO).unwrap();
        assert_eq!(categorise(&mut c, rep.batch_id).unwrap(), 0);
    }

    #[test]
    fn separate_money_in_and_out_columns_work() {
        let mut c = book();
        let mapping = serde_json::to_string(&Mapping {
            date: "Date".into(),
            money_in: Some("Paid In".into()),
            money_out: Some("Paid Out".into()),
            description: Some("Description".into()),
            ..Default::default()
        })
        .unwrap();
        c.execute(
            "INSERT INTO import_profile(id,name,date_format,mapping_json,account_id,default_currency)
             VALUES(2,'Barclays','%d/%m/%Y',?1,1,'GBP')",
            [mapping],
        )
        .unwrap();
        let csv = "Date,Description,Paid Out,Paid In\n\
                   01/08/2026,RENT,900.00,\n\
                   02/08/2026,SALARY,,2500.00\n";
        let rep = stage(&mut c, 2, "barclays.csv", csv).unwrap();
        let rows = rows(&c, rep.batch_id).unwrap();
        assert_eq!(rows[0]["amount_minor"], -90_000, "paid out is negative");
        assert_eq!(rows[1]["amount_minor"], 250_000, "paid in is positive");
    }

    #[test]
    fn a_bad_row_is_recorded_rather_than_failing_the_file() {
        let mut c = book();
        let csv = "Date,Description,Amount,Category\n\
                   01/08/2026,GOOD,-10.00,x\n\
                   not-a-date,BAD,-10.00,x\n\
                   03/08/2026,ALSO GOOD,-1.00,x\n";
        let rep = stage(&mut c, 1, "mixed.csv", csv).unwrap();
        assert_eq!(rep.rows, 3);
        assert_eq!(rep.errors, 1);
        assert_eq!(rep.new_rows, 2, "the good rows still import");
        let rows = rows(&c, rep.batch_id).unwrap();
        assert_eq!(rows[1]["state"], "error");
        assert_eq!(rows[1]["error"], "unparseable date");
    }

    #[test]
    fn quoted_fields_with_commas_survive() {
        let mut c = book();
        let csv = "Date,Description,Amount,Category\n\
                   01/08/2026,\"SMITH, JOHN & CO\",-25.00,Other\n";
        let rep = stage(&mut c, 1, "quoted.csv", csv).unwrap();
        let rows = rows(&c, rep.batch_id).unwrap();
        assert_eq!(rows[0]["description"], "SMITH, JOHN & CO");
        assert_eq!(rows[0]["amount_minor"], -2_500);
    }

    #[test]
    fn the_fingerprint_is_readable_and_stable() {
        let a = fingerprint("2026-08-01", -4_215, "TESCO STORES 3345");
        assert_eq!(a, "2026-08-01|-4215|TESCO STORES 3345");
        // whitespace and case differences between two exports of the same row must not matter
        assert_eq!(a, fingerprint("2026-08-01", -4_215, "  tesco   stores 3345 "));
        // but a different amount is a different row
        assert_ne!(a, fingerprint("2026-08-01", -4_216, "TESCO STORES 3345"));
    }
}
