//! Connection, pragmas and migrations.
//!
//! The schema ships embedded in the binary rather than read from disk, so a built `minka-ledger`
//! can create a book anywhere without carrying a migrations directory alongside it.
//!
//! Migrations are keyed on `book_meta.schema_version`, not on SQLite's `user_version`: the value is
//! then visible to anything reading the file, which matters for a format whose whole export story
//! is "hand the .db to something else and let it read".

use rusqlite::Connection;

/// Every migration in order. Index + 1 is the version it brings the book to, so appending is the
/// only supported edit -- never reorder, never rewrite a landed one.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_txn_link.sql"),
    include_str!("../migrations/0003_role_account_pins.sql"),
    include_str!("../migrations/0004_series_chain.sql"),
];

pub fn current_version() -> i64 {
    MIGRATIONS.len() as i64
}

#[derive(Debug)]
pub enum DbError {
    Sql(rusqlite::Error),
    /// The file was written by a newer build. Refuse rather than guess: a forward migration we do
    /// not have could mean columns we would silently ignore.
    FromTheFuture { found: i64, known: i64 },
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sql(e) => write!(f, "{e}"),
            DbError::FromTheFuture { found, known } => write!(
                f,
                "book is schema version {found} but this build only knows {known} -- upgrade minka-ledger"
            ),
        }
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sql(e)
    }
}

/// Open (creating if absent) and bring the book up to date.
pub fn open(path: &str) -> Result<Connection, DbError> {
    let conn = Connection::open(path)?;
    // WAL survives a crash mid-write without losing the book; foreign_keys is OFF by default in
    // SQLite and every composite-FK guarantee in the schema depends on it being ON.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

fn read_version(conn: &Connection) -> i64 {
    // Absent table or absent row both mean "brand new book".
    conn.query_row("SELECT value FROM book_meta WHERE key = 'schema_version'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0)
}

pub(crate) fn migrate(conn: &Connection) -> Result<(), DbError> {
    let found = read_version(conn);
    let known = current_version();
    if found > known {
        return Err(DbError::FromTheFuture { found, known });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let version = i as i64 + 1;
        // One transaction per migration: a failure leaves the book at the last good version rather
        // than half-applied.
        conn.execute_batch("BEGIN")?;
        // execute_batch on the migration itself would end the transaction if the SQL contains its
        // own COMMIT; the schema deliberately contains none.
        if let Err(e) = conn.execute_batch(sql) {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(DbError::Sql(e));
        }
        conn.execute(
            "INSERT INTO book_meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [version.to_string()],
        )?;
        conn.execute_batch("COMMIT")?;
    }
    Ok(())
}

/// The three integrity views the schema defines. Any non-empty result is a book-level bug, not a
/// user error -- nothing the operator can type should be able to produce one.
pub fn integrity(conn: &Connection) -> Result<serde_json::Value, DbError> {
    let mut out = serde_json::Map::new();
    for view in ["v_check_txn_unbalanced", "v_check_book_unbalanced", "v_check_missing_conversion"] {
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {view}"), [], |r| r.get(0))?;
        out.insert(view.to_string(), serde_json::json!(n));
    }
    let ok = out.values().all(|v| v.as_i64() == Some(0));
    out.insert("ok".to_string(), serde_json::json!(ok));
    Ok(serde_json::Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn migrates_a_fresh_book_and_is_idempotent() {
        let conn = mem();
        assert_eq!(read_version(&conn), current_version());
        migrate(&conn).unwrap(); // running again must be a no-op, not an error
        assert_eq!(read_version(&conn), current_version());
    }

    #[test]
    fn a_fresh_book_is_internally_consistent() {
        let conn = mem();
        let report = integrity(&conn).unwrap();
        assert_eq!(report["ok"], serde_json::json!(true), "{report}");
    }

    #[test]
    fn refuses_a_book_from_a_newer_build() {
        let conn = mem();
        conn.execute(
            "UPDATE book_meta SET value = ?1 WHERE key = 'schema_version'",
            [(current_version() + 5).to_string()],
        )
        .unwrap();
        match migrate(&conn) {
            Err(DbError::FromTheFuture { found, known }) => {
                assert_eq!(found, current_version() + 5);
                assert_eq!(known, current_version());
            }
            other => panic!("expected FromTheFuture, got {other:?}"),
        }
    }
}
