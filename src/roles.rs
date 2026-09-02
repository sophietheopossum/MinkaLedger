//! Durable identity for the three accounts the CORE owns rather than the operator.
//!
//! Most accounts exist because someone typed them, and the core only ever reaches those by id. Three
//! do not: the per-currency FX trading accounts, the per-currency counterweight every opening
//! balance is posted against, and the bucket the importer files rows it cannot categorise into. The
//! core creates those on demand and has to find them again later.
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
//!   unclassified  `book_meta` key `unclassified_account`.
//!
//! The two pins are written whenever the account is created OR first resolved, and migration 0003
//! backfills them for books that already have these accounts -- it runs inside `db::open`, so a book
//! is pinned before any handler gets the chance to rename anything. A pin is VALIDATED on read (the
//! row still exists, and is still the right kind and currency); a stale one falls back to the
//! creating path rather than resolving to whatever now holds that id.
//!
//! The names below are therefore only what these accounts are CREATED with. Nothing looks them up by
//! that afterwards, which is what makes renaming the counterweight the purely cosmetic act
//! `account.rename` claims it is. `reserved_for` still refuses to hand one of those names to an
//! ordinary account: after the change a squat can no longer redirect money, but it would collide
//! with the UNIQUE index the first time the core tried to create the real one, and "that name is
//! ours" is a better answer than a constraint failure two operations later.

use rusqlite::Connection;

pub const UNCLASSIFIED_KEY: &str = "unclassified_account";
pub const UNCLASSIFIED_NAME: &str = "Expenses:Unclassified";

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

/// Resolve the unclassified bucket WITHOUT creating one, and return its current name with it.
///
/// Readers need both halves: analysis.brief measures the account by id, but has to call it by
/// whatever it is now called, or its caveat would name an account the operator cannot find. `None`
/// means the book has no such account, which is the ordinary state of a book that has never
/// imported anything -- not an error.
pub fn unclassified_existing(conn: &Connection) -> Option<(i64, String)> {
    if let Some(id) = pinned(conn, UNCLASSIFIED_KEY) {
        let live = conn
            .query_row(
                "SELECT id, name FROM account WHERE id = ?1 AND kind = 'expense'",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if live.is_some() {
            return live;
        }
    }
    conn.query_row(
        "SELECT id, name FROM account WHERE name = ?1 AND kind = 'expense'",
        [UNCLASSIFIED_NAME],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// The same account, created if the book has none yet. For writers -- the importer, which has to put
/// an uncategorised row somewhere.
pub fn unclassified(conn: &Connection) -> rusqlite::Result<i64> {
    if let Some((id, _)) = unclassified_existing(conn) {
        pin(conn, UNCLASSIFIED_KEY, id)?;
        return Ok(id);
    }
    let cur: String =
        conn.query_row("SELECT value FROM book_meta WHERE key='display_currency'", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO account(name, kind, currency) VALUES(?1,'expense',?2)",
        rusqlite::params![UNCLASSIFIED_NAME, cur],
    )?;
    let id = conn.last_insert_rowid();
    pin(conn, UNCLASSIFIED_KEY, id)?;
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
    } else if name == UNCLASSIFIED_NAME {
        Some("the account the importer files rows it cannot categorise into")
    } else {
        None
    }
}
