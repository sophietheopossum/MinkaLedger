//! Durable identity for the three accounts the CORE owns rather than the operator.
//!
//! Most accounts exist because someone typed them, and the core only ever reaches those by id. Three
//! do not: the per-currency FX trading accounts, the per-currency counterweight every opening
//! balance is posted against, and the per-currency bucket the importer files rows it cannot
//! categorise into. The core creates those on demand and has to find them again later.
//!
//! It used to find them by their literal NAME, which was safe for exactly as long as no name could
//! change. `account.rename` made names editable, and a name-keyed lookup then has two silent failure
//! modes, both of which leave `db.check` green because the arithmetic never stops adding up:
//!
//!   MISS -- the account is renamed, so the lookup no longer sees it. The next opening balance grows
//!   a SECOND counterweight, and `analysis.brief` stops reporting uncategorised spend entirely: the
//!   money is still there and still uncategorised, but the caveat that says every category total is
//!   wrong by that much simply disappears.
//!
//!   HIT -- an ordinary account that already holds money is renamed ONTO the reserved name, and
//!   starts receiving conversion legs or opening counterweights. A real EUR savings account renamed
//!   to `Conversion:EUR` ends up permanently negative by the whole conversion volume, and
//!   `v_check_missing_conversion` stays at zero throughout because the other leg did hit a genuine
//!   conversion account.
//!
//! So none of the three is name-keyed any more:
//!
//!   conversion    `kind = 'conversion' AND currency = ?`. The schema's
//!                 `CHECK (kind <> 'conversion' OR system = 1)` makes that kind unreachable from
//!                 `account.create` (which never sets `system`), so the shape alone is proof of
//!                 origin and this one needs no stored pin at all.
//!   equity        `book_meta` key `opening_equity.<CUR>`.
//!   unclassified  `book_meta` key `unclassified_account.<CUR>`.
//!
//! The two pins are written whenever the account is created OR first resolved, and migration 0003
//! backfills them for books that already have these accounts -- it runs inside `db::open`, so a book
//! is pinned before any handler gets the chance to rename anything. A pin is VALIDATED on read (the
//! row still exists, and is still the right kind and currency); a stale one falls back to the
//! creating path rather than resolving to whatever now holds that id.
//!
//! The unclassified bucket was ONE account, in the display currency, until migration 0005. A posting
//! is in its account's currency, so that bucket could not balance a row in any other, and an
//! uncategorised line of a foreign statement had nowhere to go. Each currency now has its own, as
//! each has its own counterweight. 0005 moves the old pin under the key for the currency its account
//! holds, so the bucket a book already has goes on serving that currency under whatever it is called.
//!
//! The names below are therefore only what these accounts are CREATED with. Nothing looks them up by
//! that afterwards, which is what makes renaming the counterweight the purely cosmetic act
//! `account.rename` claims it is. `reserved_for` still refuses to hand one of those names to an
//! ordinary account: after the change a squat can no longer redirect money, but it would collide
//! with the UNIQUE index the first time the core tried to create the real one, and "that name is
//! ours" is a better answer than a constraint failure two operations later.

use rusqlite::Connection;

/// What the bucket was created as while a book had only one. An account still called this goes on
/// serving its own currency; nothing new is created under the bare name.
pub const UNCLASSIFIED_NAME: &str = "Expenses:Unclassified";

pub fn unclassified_key(cur: &str) -> String {
    format!("unclassified_account.{cur}")
}

/// What a bucket is CREATED as, in the shape of the counterweight's name. A label from then on.
pub fn unclassified_name(cur: &str) -> String {
    format!("{UNCLASSIFIED_NAME} ({cur})")
}

/// `book_meta` is TEXT-valued and the schema is STRICT, so ids are stored as text and cast back.
fn pinned(conn: &Connection, key: &str) -> Option<i64> {
    conn.query_row("SELECT CAST(value AS INTEGER) FROM book_meta WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .ok()
}

fn pin(conn: &Connection, key: &str, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO book_meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, id.to_string()],
    )?;
    Ok(())
}

pub fn opening_equity_key(cur: &str) -> String {
    format!("opening_equity.{cur}")
}

/// What a counterweight is CREATED as. It is a label from that moment on -- the operator may rename
/// it, and `opening_equity` will still find it.
pub fn opening_equity_name(cur: &str) -> String {
    format!("Opening balances ({cur})")
}

fn opening_equity_id(conn: &Connection, cur: &str) -> Option<i64> {
    if let Some(id) = pinned(conn, &opening_equity_key(cur)) {
        // Validated, not trusted: an id that no longer names an equity account in this currency is
        // a pin left over from something deleted, and reusing it would post the counterweight into
        // whatever inherited the row.
        let live: Option<i64> = conn
            .query_row(
                "SELECT id FROM account WHERE id = ?1 AND kind = 'equity' AND currency = ?2",
                rusqlite::params![id, cur],
                |r| r.get(0),
            )
            .ok();
        if live.is_some() {
            return live;
        }
    }
    // A book whose counterweight predates the pin and that migration 0003 could not match -- the
    // name is still the best evidence available, but it is only consulted once, and only alongside
    // the kind, so it can never resolve to an ordinary asset account squatting on the string.
    conn.query_row(
        "SELECT id FROM account WHERE kind = 'equity' AND currency = ?1 AND name = ?2",
        rusqlite::params![cur, opening_equity_name(cur)],
        |r| r.get(0),
    )
    .ok()
}

/// The equity account every opening balance in `cur` is counterweighted against, created on first
/// use. One per currency, because a transaction balances per currency and the composite FK ties a
/// posting's currency to its account's.
pub fn opening_equity(conn: &Connection, cur: &str) -> rusqlite::Result<i64> {
    if let Some(id) = opening_equity_id(conn, cur) {
        pin(conn, &opening_equity_key(cur), id)?;
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO account(name, kind, currency) VALUES(?1,'equity',?2)",
        rusqlite::params![opening_equity_name(cur), cur],
    )?;
    let id = conn.last_insert_rowid();
    pin(conn, &opening_equity_key(cur), id)?;
    Ok(id)
}

/// The bucket for `cur` and its current name, WITHOUT creating one.
fn unclassified_in(conn: &Connection, cur: &str) -> Option<(i64, String)> {
    if let Some(id) = pinned(conn, &unclassified_key(cur)) {
        // Validated, not trusted, exactly as the counterweight's pin is: an id that no longer names
        // an expense account in this currency was left by something deleted.
        let live = conn
            .query_row(
                "SELECT id, name FROM account WHERE id = ?1 AND kind = 'expense' AND currency = ?2",
                rusqlite::params![id, cur],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if live.is_some() {
            return live;
        }
    }
    // By name only when no pin answers, and only alongside the kind and the currency: either what
    // the core creates now, or the bare name the single bucket of an older book was created with.
    conn.query_row(
        "SELECT id, name FROM account
          WHERE kind = 'expense' AND currency = ?1 AND name IN (?2, ?3)
          ORDER BY id LIMIT 1",
        rusqlite::params![cur, unclassified_name(cur), UNCLASSIFIED_NAME],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// Every unclassified bucket the book has, WITHOUT creating any, each with its current name.
///
/// Readers need both halves: analysis.brief measures each account by id, but has to call it by
/// whatever it is now called, or its caveat would name an account the operator cannot find. An empty
/// list is the ordinary state of a book that has never imported anything -- not an error.
pub fn unclassified_existing(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let currencies: Vec<String> = conn
        .prepare("SELECT code FROM currency ORDER BY code")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(currencies.iter().filter_map(|cur| unclassified_in(conn, cur)).collect())
}

/// The bucket for `cur`, created if the book has none yet. For writers -- the importer, which has to
/// put an uncategorised row somewhere.
///
/// One per currency for the reason there is one counterweight per currency: a transaction balances
/// per currency, and the composite FK ties a posting's currency to its account's.
pub fn unclassified(conn: &Connection, cur: &str) -> rusqlite::Result<i64> {
    if let Some((id, _)) = unclassified_in(conn, cur) {
        pin(conn, &unclassified_key(cur), id)?;
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO account(name, kind, currency) VALUES(?1,'expense',?2)",
        rusqlite::params![unclassified_name(cur), cur],
    )?;
    let id = conn.last_insert_rowid();
    pin(conn, &unclassified_key(cur), id)?;
    Ok(id)
}

pub fn conversion_name(code: &str) -> String {
    format!("Conversion:{code}")
}

/// The per-currency trading account, created on first use. `system = 1` keeps every other writer
/// out: entry.rs refuses postings to it, so only fx.rs can move money through.
///
/// No pin: `kind = 'conversion'` is already an identity nothing outside this function can forge,
/// because the schema requires `system = 1` for that kind and `account.create` never sets it.
pub fn conversion(conn: &Connection, code: &str) -> rusqlite::Result<i64> {
    if let Ok(id) = conn.query_row(
        "SELECT id FROM account WHERE kind = 'conversion' AND currency = ?1 ORDER BY id LIMIT 1",
        [code],
        |r| r.get(0),
    ) {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO account(name, kind, currency, system) VALUES(?1,'conversion',?2,1)",
        rusqlite::params![conversion_name(code), code],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Is this a name the core creates for itself, and if so what for?
///
/// Answers in words the operator can act on, because it is shown to them verbatim. Matched on the
/// SHAPE rather than against a list of live accounts: `Conversion:JPY` is the core's name whether or
/// not the book has ever held a yen, and refusing it only once it exists would let a squat win
/// simply by being first.
pub fn reserved_for(name: &str) -> Option<&'static str> {
    if name.starts_with("Conversion:") {
        Some("the per-currency account the core posts the two halves of a currency conversion through")
    } else if name.starts_with("Opening balances (") {
        Some("the per-currency counterweight the core posts opening balances against")
    } else if name == UNCLASSIFIED_NAME || name.starts_with("Expenses:Unclassified (") {
        Some("the per-currency account the importer files rows it cannot categorise into")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(conn: &Connection, id: i64) -> String {
        conn.query_row("SELECT name FROM account WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn each_currency_has_its_own_bucket_and_finds_it_again_under_any_name() {
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrate(&c).unwrap();
        assert!(unclassified_existing(&c).unwrap().is_empty(), "a book that has imported nothing");

        let gbp = unclassified(&c, "GBP").unwrap();
        let eur = unclassified(&c, "EUR").unwrap();
        assert_ne!(gbp, eur);
        assert_eq!(name_of(&c, eur), "Expenses:Unclassified (EUR)");
        c.execute("UPDATE account SET name = 'Euro to-do' WHERE id = ?1", [eur]).unwrap();
        assert_eq!(unclassified(&c, "EUR").unwrap(), eur, "found by its pin, not created again");
        assert_eq!(
            unclassified_existing(&c).unwrap(),
            vec![(eur, "Euro to-do".to_string()), (gbp, "Expenses:Unclassified (GBP)".to_string())]
        );
    }

    #[test]
    fn the_single_bucket_of_an_older_book_goes_on_serving_its_own_currency() {
        // A book as 0004 left it: one bucket, pinned under the one key, and renamed since -- so
        // nothing but the pin says which account it is.
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        for sql in [
            include_str!("../migrations/0001_init.sql"),
            include_str!("../migrations/0002_txn_link.sql"),
            include_str!("../migrations/0003_role_account_pins.sql"),
            include_str!("../migrations/0004_series_chain.sql"),
        ] {
            c.execute_batch(sql).unwrap();
        }
        c.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES(7,'Misc spending','expense','GBP');
             INSERT INTO book_meta(key,value) VALUES('unclassified_account','7');",
        )
        .unwrap();
        c.execute_batch(include_str!("../migrations/0005_unclassified_per_currency.sql")).unwrap();

        assert_eq!(unclassified(&c, "GBP").unwrap(), 7, "no second GBP bucket beside the renamed one");
        let eur = unclassified(&c, "EUR").unwrap();
        assert_eq!(name_of(&c, eur), "Expenses:Unclassified (EUR)");
        let old_keys: i64 = c
            .query_row("SELECT COUNT(*) FROM book_meta WHERE key = 'unclassified_account'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(old_keys, 0);
    }
}
