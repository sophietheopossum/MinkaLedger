//! Multi-currency: exact-rational rates, and conversions that still balance.
//!
//! TWO KINDS OF RATE, and conflating them is the classic mistake.
//!
//! An EXECUTED rate is the one a conversion actually got. It is never stored as a number -- it is
//! the ratio of the two integer legs, and asking for it is a division. That is what makes a
//! conversion have no rounding residual at all: both sides are exact integers, so the sub-penny
//! lives in the effective rate rather than in a stray posting nobody can explain.
//!
//! A MARKET rate is ambient and fluctuating, used to VALUE holdings for a report. Those live in
//! `fx_rate` as exact rationals (`num`/`den`), never floats, never a fixed scale.
//!
//! HOW A CONVERSION BALANCES. A txn must sum to zero per currency, but a GBP->EUR conversion
//! cannot. So the core adds postings to per-currency `Conversion:<CUR>` accounts -- GnuCash's
//! trading accounts. £400 out of Current and €466.64 into Euro pot becomes four postings: -40000
//! GBP Current, +40000 GBP Conversion:GBP, +46664 EUR Euro pot, -46664 EUR Conversion:EUR. Both
//! currencies balance, and the pair of conversion accounts holds the FX story: revalue them at
//! today's rate and the difference is your gain or loss, as a balance query rather than bespoke
//! report code.

use chrono::NaiveDate;
use rusqlite::Connection;

use crate::money::{round_half_away, Minor};

#[derive(Debug)]
pub enum FxError {
    Sql(rusqlite::Error),
    NoRate { base: String, quote: String, on: String },
    UnknownCurrency(String),
    SameCurrency(String),
    BadRate(String),
}

impl std::fmt::Display for FxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FxError::Sql(e) => write!(f, "{e}"),
            FxError::NoRate { base, quote, on } => {
                write!(f, "no {base}/{quote} rate on or before {on}")
            }
            FxError::UnknownCurrency(c) => write!(f, "unknown currency: {c}"),
            FxError::SameCurrency(c) => write!(f, "{c} to itself is not a conversion"),
            FxError::BadRate(m) => write!(f, "bad rate: {m}"),
        }
    }
}

impl From<rusqlite::Error> for FxError {
    fn from(e: rusqlite::Error) -> Self {
        FxError::Sql(e)
    }
}

/// An exact rational rate: 1 `base` = num/den `quote`, in MAJOR units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub num: i64,
    pub den: i64,
}

impl Rate {
    pub fn as_f64_for_display(&self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

pub fn display_currency(conn: &Connection) -> Result<String, FxError> {
    Ok(conn.query_row("SELECT value FROM book_meta WHERE key='display_currency'", [], |r| r.get(0))?)
}

fn minor_digits(conn: &Connection, code: &str) -> Result<u32, FxError> {
    conn.query_row("SELECT minor_digits FROM currency WHERE code = ?1", [code], |r| {
        r.get::<_, i64>(0)
    })
    .map(|d| d as u32)
    .map_err(|_| FxError::UnknownCurrency(code.to_string()))
}

/// Store a rate. `as_of` must be the date the SOURCE reports, not the date asked for -- the Bank of
/// England returns Friday's rate for a weekend query, and recording it under Saturday would invent
/// a rate that was never published.
pub fn put_rate(
    conn: &Connection,
    base: &str,
    quote: &str,
    as_of: &str,
    source: &str,
    rate: Rate,
) -> Result<(), FxError> {
    if base == quote {
        return Err(FxError::SameCurrency(base.to_string()));
    }
    if rate.num <= 0 || rate.den <= 0 {
        return Err(FxError::BadRate(format!("{}/{}", rate.num, rate.den)));
    }
    conn.execute(
        "INSERT INTO fx_rate(base_code, quote_code, as_of, source, num, den, fetched_at)
         VALUES(?1,?2,?3,?4,?5,?6, datetime('now'))
         ON CONFLICT(base_code, quote_code, as_of, source)
         DO UPDATE SET num=excluded.num, den=excluded.den, fetched_at=excluded.fetched_at",
        rusqlite::params![base, quote, as_of, source, rate.num, rate.den],
    )?;
    Ok(())
}

/// The rate in force for `base`->`quote` on `on`: the latest one dated on or before it.
///
/// ONE code path serves history and the forecast. For a past date it finds the rate that applied;
/// for a future date it finds the most recent known spot and holds it flat. That is not a
/// simplification to apologise for -- a projection has no business predicting FX, and a flat carry
/// is the honest assumption, made visible by `as_of` in the result.
pub fn resolve_rate(
    conn: &Connection,
    base: &str,
    quote: &str,
    on: NaiveDate,
) -> Result<(Rate, String), FxError> {
    if base == quote {
        return Ok((Rate { num: 1, den: 1 }, on.to_string()));
    }
    let direct = conn
        .query_row(
            "SELECT num, den, as_of FROM fx_rate
              WHERE base_code = ?1 AND quote_code = ?2 AND as_of <= ?3
              ORDER BY as_of DESC,
                       CASE source WHEN 'manual' THEN 0 WHEN 'boe' THEN 1 ELSE 2 END
              LIMIT 1",
            rusqlite::params![base, quote, on.to_string()],
            |r| Ok((Rate { num: r.get(0)?, den: r.get(1)? }, r.get::<_, String>(2)?)),
        )
        .ok();
    if let Some(hit) = direct {
        return Ok(hit);
    }
    // Stored the other way round: invert exactly by swapping the rational's parts. No precision is
    // lost, which is the whole reason rates are rationals and not decimals.
    let inverse = conn
        .query_row(
            "SELECT num, den, as_of FROM fx_rate
              WHERE base_code = ?2 AND quote_code = ?1 AND as_of <= ?3
              ORDER BY as_of DESC,
                       CASE source WHEN 'manual' THEN 0 WHEN 'boe' THEN 1 ELSE 2 END
              LIMIT 1",
            rusqlite::params![base, quote, on.to_string()],
            |r| Ok((Rate { num: r.get(1)?, den: r.get(0)? }, r.get::<_, String>(2)?)),
        )
        .ok();
    if let Some(hit) = inverse {
        return Ok(hit);
    }
    // Cross via the display currency: EUR->USD = (GBP->USD) / (GBP->EUR).
    let base_ccy = display_currency(conn)?;
    if base != base_ccy && quote != base_ccy {
        let (to_quote, d1) = resolve_rate(conn, &base_ccy, quote, on)?;
        let (to_base, d2) = resolve_rate(conn, &base_ccy, base, on)?;
        return Ok((
            Rate { num: to_quote.num * to_base.den, den: to_quote.den * to_base.num },
            if d1 < d2 { d1 } else { d2 },
        ));
    }
    Err(FxError::NoRate {
        base: base.to_string(),
        quote: quote.to_string(),
        on: on.to_string(),
    })
}

/// Convert an amount between currencies of possibly different scales.
///
/// The scale change is part of the arithmetic, not an afterthought: 100 JPY (0dp) into GBP (2dp)
/// must multiply by 100 as well as by the rate, and doing that in one `round_half_away` means one
/// rounding rather than two.
pub fn convert_minor(
    amount: Minor,
    rate: Rate,
    from_digits: u32,
    to_digits: u32,
) -> Result<Minor, FxError> {
    let scale_up = 10i128.pow(to_digits);
    let scale_down = 10i128.pow(from_digits);
    round_half_away(
        amount as i128 * rate.num as i128 * scale_up,
        rate.den as i128 * scale_down,
    )
    .map_err(|e| FxError::BadRate(e.to_string()))
}

pub fn convert(
    conn: &Connection,
    amount: Minor,
    from: &str,
    to: &str,
    on: NaiveDate,
) -> Result<(Minor, Rate, String), FxError> {
    let (rate, as_of) = resolve_rate(conn, from, to, on)?;
    let out = convert_minor(amount, rate, minor_digits(conn, from)?, minor_digits(conn, to)?)?;
    Ok((out, rate, as_of))
}

/// Find (creating if needed) the per-currency conversion account.
///
/// It used to be found by the literal name `Conversion:<CUR>`, which stopped being safe the moment
/// `account.rename` existed: an ordinary EUR account renamed to `Conversion:EUR` before the book's
/// first conversion would have received the conversion leg and sat permanently negative by the whole
/// conversion volume, with `v_check_missing_conversion` still reading zero because the other side
/// hit a real one. crate::roles matches on `kind = 'conversion'` instead, which the schema's
/// `CHECK (kind <> 'conversion' OR system = 1)` makes unreachable from `account.create`.
fn conversion_account(conn: &Connection, code: &str) -> Result<i64, FxError> {
    Ok(crate::roles::conversion(conn, code)?)
}

/// Build a currency conversion as one balanced transaction.
///
/// `from_minor` is what leaves (positive), `to_minor` what arrives. Both are given explicitly and
/// exactly: the caller knows what actually happened, and deriving one from a rate would reintroduce
/// the sub-penny residual this design exists to avoid.
#[allow(clippy::too_many_arguments)]
pub fn build_conversion(
    conn: &mut Connection,
    occurred_on: &str,
    description: &str,
    from_account: i64,
    from_minor: Minor,
    to_account: i64,
    to_minor: Minor,
) -> Result<i64, FxError> {
    let from_cur: String =
        conn.query_row("SELECT currency FROM account WHERE id=?1", [from_account], |r| r.get(0))?;
    let to_cur: String =
        conn.query_row("SELECT currency FROM account WHERE id=?1", [to_account], |r| r.get(0))?;
    if from_cur == to_cur {
        return Err(FxError::SameCurrency(from_cur));
    }
    let conv_from = conversion_account(conn, &from_cur)?;
    let conv_to = conversion_account(conn, &to_cur)?;

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO txn(occurred_on, description, source) VALUES(?1,?2,'manual')",
        rusqlite::params![occurred_on, description],
    )?;
    let txn_id = tx.last_insert_rowid();
    {
        let mut st = tx.prepare(
            "INSERT INTO posting(txn_id, account_id, currency, amount_minor) VALUES(?1,?2,?3,?4)",
        )?;
        // Each currency balances on its own: the money leaving is offset inside its own conversion
        // account, and likewise for the money arriving.
        st.execute(rusqlite::params![txn_id, from_account, from_cur, -from_minor])?;
        st.execute(rusqlite::params![txn_id, conv_from, from_cur, from_minor])?;
        st.execute(rusqlite::params![txn_id, to_account, to_cur, to_minor])?;
        st.execute(rusqlite::params![txn_id, conv_to, to_cur, -to_minor])?;
    }
    tx.commit()?;
    Ok(txn_id)
}

/// What the conversion accounts say the FX has cost or made, valued at `on`.
///
/// Each conversion account holds the gross flow in its own currency. Valued in the display
/// currency at one date, the pair sums to the gain or loss -- no lot tracking, no bespoke report.
pub fn fx_position(conn: &Connection, on: NaiveDate) -> Result<serde_json::Value, FxError> {
    let display = display_currency(conn)?;
    let mut stmt = conn.prepare(
        "SELECT a.currency, COALESCE(SUM(p.amount_minor),0)
           FROM account a JOIN posting p ON p.account_id = a.id
           JOIN txn t ON t.id = p.txn_id
          WHERE a.kind = 'conversion' AND t.occurred_on <= ?1
          GROUP BY a.currency",
    )?;
    let legs: Vec<(String, Minor)> = stmt
        .query_map([on.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    let mut total: Minor = 0;
    let mut detail = Vec::new();
    for (cur, amount) in &legs {
        let valued = if *cur == display {
            *amount
        } else {
            convert(conn, *amount, cur, &display, on)?.0
        };
        total += valued;
        detail.push(serde_json::json!({
            "currency": cur, "amount_minor": amount, "valued_minor": valued,
        }));
    }
    Ok(serde_json::json!({
        "as_of": on.to_string(),
        "display_currency": display,
        // Conversion accounts net to zero at the rate used AT THE TIME; a non-zero total is the
        // movement since. Sign convention: positive is a gain in the display currency.
        "gain_minor": -total,
        "legs": detail,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn book() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql")).unwrap();
        conn.execute_batch(
            "INSERT INTO account(id,name,kind,currency) VALUES
               (1,'Current','asset','GBP'),
               (2,'Euro pot','asset','EUR'),
               (3,'Yen pot','asset','JPY');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn rates_are_exact_rationals_and_invert_losslessly() {
        let c = book();
        // 1 GBP = 1.1666 EUR
        put_rate(&c, "GBP", "EUR", "2026-08-01", "manual", Rate { num: 11_666, den: 10_000 }).unwrap();
        let (fwd, _) = resolve_rate(&c, "GBP", "EUR", d("2026-08-01")).unwrap();
        assert_eq!(fwd, Rate { num: 11_666, den: 10_000 });
        // the reverse direction is the same rational upside down -- no precision lost
        let (rev, _) = resolve_rate(&c, "EUR", "GBP", d("2026-08-01")).unwrap();
        assert_eq!(rev, Rate { num: 10_000, den: 11_666 });
    }

    #[test]
    fn resolution_takes_the_latest_rate_on_or_before_the_date() {
        let c = book();
        put_rate(&c, "GBP", "EUR", "2026-08-01", "boe", Rate { num: 11_600, den: 10_000 }).unwrap();
        put_rate(&c, "GBP", "EUR", "2026-08-15", "boe", Rate { num: 11_800, den: 10_000 }).unwrap();
        let (r, as_of) = resolve_rate(&c, "GBP", "EUR", d("2026-08-10")).unwrap();
        assert_eq!(r.num, 11_600);
        assert_eq!(as_of, "2026-08-01", "the rate that was in force, not the one asked for");
        // and a FUTURE date holds the last known rate flat, from the same code path
        let (r, as_of) = resolve_rate(&c, "GBP", "EUR", d("2027-06-01")).unwrap();
        assert_eq!(r.num, 11_800);
        assert_eq!(as_of, "2026-08-15", "flat carry, and it says which date it came from");
    }

    #[test]
    fn a_manual_rate_beats_a_fetched_one_on_the_same_day() {
        let c = book();
        put_rate(&c, "GBP", "EUR", "2026-08-01", "ecb", Rate { num: 11_600, den: 10_000 }).unwrap();
        put_rate(&c, "GBP", "EUR", "2026-08-01", "manual", Rate { num: 11_900, den: 10_000 }).unwrap();
        assert_eq!(resolve_rate(&c, "GBP", "EUR", d("2026-08-01")).unwrap().0.num, 11_900);
    }

    #[test]
    fn a_missing_rate_is_an_error_not_a_guess() {
        let c = book();
        put_rate(&c, "GBP", "EUR", "2026-08-01", "boe", Rate { num: 11_666, den: 10_000 }).unwrap();
        // asking before any rate exists must fail rather than extrapolate backwards
        assert!(matches!(
            resolve_rate(&c, "GBP", "EUR", d("2026-07-01")),
            Err(FxError::NoRate { .. })
        ));
    }

    #[test]
    fn converts_across_different_minor_scales() {
        let c = book();
        // JPY has 0 decimal places; GBP has 2. 1 GBP = 190 JPY.
        put_rate(&c, "GBP", "JPY", "2026-08-01", "manual", Rate { num: 190, den: 1 }).unwrap();
        // £10.00 -> 1900 yen (as 1900 minor units, since JPY minor_digits = 0)
        let (out, _, _) = convert(&c, 1_000, "GBP", "JPY", d("2026-08-01")).unwrap();
        assert_eq!(out, 1_900);
        // and back
        let (back, _, _) = convert(&c, 1_900, "JPY", "GBP", d("2026-08-01")).unwrap();
        assert_eq!(back, 1_000);
    }

    #[test]
    fn a_cross_rate_goes_through_the_display_currency() {
        let c = book();
        put_rate(&c, "GBP", "EUR", "2026-08-01", "boe", Rate { num: 12_000, den: 10_000 }).unwrap();
        put_rate(&c, "GBP", "JPY", "2026-08-01", "boe", Rate { num: 190, den: 1 }).unwrap();
        // EUR->JPY = (GBP->JPY) / (GBP->EUR) = 190 / 1.2 = 158.33...
        let (r, _) = resolve_rate(&c, "EUR", "JPY", d("2026-08-01")).unwrap();
        assert!((r.as_f64_for_display() - 158.3333).abs() < 0.001, "got {r:?}");
    }

    #[test]
    fn a_conversion_balances_per_currency_with_no_residual() {
        let mut c = book();
        // £400.00 out, €466.64 in -- both exact, so no sub-penny anywhere.
        let id = build_conversion(&mut c, "2026-08-01", "GBP->EUR", 1, 40_000, 2, 46_664).unwrap();
        let rows: Vec<(String, i64)> = c
            .prepare("SELECT currency, SUM(amount_minor) FROM posting WHERE txn_id=?1 GROUP BY currency")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (cur, sum) in rows {
            assert_eq!(sum, 0, "{cur} must balance");
        }
        // and the book-wide integrity views agree
        let report = crate::db::integrity(&c).unwrap();
        assert_eq!(report["ok"], serde_json::json!(true), "{report}");
    }

    #[test]
    fn conversion_accounts_are_created_once_and_are_system_owned() {
        let mut c = book();
        build_conversion(&mut c, "2026-08-01", "a", 1, 10_000, 2, 11_666).unwrap();
        build_conversion(&mut c, "2026-08-02", "b", 1, 10_000, 2, 11_700).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM account WHERE kind='conversion'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "one per currency, reused");
        let sys: i64 = c
            .query_row("SELECT MIN(system) FROM account WHERE kind='conversion'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sys, 1, "and locked against hand posting");
    }

    #[test]
    fn a_same_currency_conversion_is_refused() {
        let mut c = book();
        assert!(matches!(
            build_conversion(&mut c, "2026-08-01", "x", 1, 100, 1, 100),
            Err(FxError::SameCurrency(_))
        ));
    }

    #[test]
    fn fx_gain_falls_out_of_the_conversion_accounts() {
        let mut c = book();
        // Convert £400 into €466.64 at 1.1666.
        put_rate(&c, "GBP", "EUR", "2026-08-01", "boe", Rate { num: 11_666, den: 10_000 }).unwrap();
        build_conversion(&mut c, "2026-08-01", "GBP->EUR", 1, 40_000, 2, 46_664).unwrap();

        // Valued at the SAME rate, the position is flat: nothing has happened yet.
        let flat = fx_position(&c, d("2026-08-01")).unwrap();
        assert_eq!(flat["gain_minor"], 0, "no movement, no gain: {flat}");

        // The euro strengthens: 1 GBP now buys only 1.10 EUR, so the euros are worth more.
        put_rate(&c, "GBP", "EUR", "2026-09-01", "boe", Rate { num: 11_000, den: 10_000 }).unwrap();
        let later = fx_position(&c, d("2026-09-01")).unwrap();
        let gain = later["gain_minor"].as_i64().unwrap();
        assert!(gain > 0, "a stronger euro is a gain on a euro holding: {later}");
    }

    #[test]
    fn rounding_of_a_conversion_is_half_away_from_zero() {
        // 1 GBP = 1.005 EUR, converting £1.00 -> €1.005 -> rounds to €1.01, not €1.00.
        let r = Rate { num: 1_005, den: 1_000 };
        assert_eq!(convert_minor(100, r, 2, 2).unwrap(), 101);
        // and symmetrically for a negative amount
        assert_eq!(convert_minor(-100, r, 2, 2).unwrap(), -101);
    }
}

/// Fetching rates from Frankfurter (ECB reference rates, free, no key).
///
/// The date stored is ALWAYS the one the API reports, never the one requested. Verified 28/8/2026:
/// asking for Sunday 2026-08-30 returns `"date":"2026-08-28"`, Friday's rate -- the ECB does not
/// publish at weekends. Recording that under Sunday would invent a rate that never existed, and
/// every later valuation on that date would silently cite it.
pub mod fetch {
    use super::*;

    const BASE: &str = "https://api.frankfurter.dev/v1";

    /// Parse a decimal like "1.1666" into an exact rational, losslessly. No float ever holds it.
    pub fn decimal_to_rate(text: &str) -> Result<Rate, FxError> {
        let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
        if frac.len() > 18 {
            return Err(FxError::BadRate(format!("{text}: too many decimal places")));
        }
        let digits = format!("{whole}{frac}");
        let num: i64 = digits.parse().map_err(|_| FxError::BadRate(text.to_string()))?;
        Ok(Rate { num, den: 10i64.pow(frac.len() as u32) })
    }

    /// Fetch `base` against every currency in the book for `on`, and store what comes back.
    /// Returns the date the source reported and how many rates landed.
    pub fn rates_for(conn: &Connection, on: NaiveDate) -> Result<(String, usize), FxError> {
        let base = display_currency(conn)?;
        let mut symbols: Vec<String> = conn
            .prepare("SELECT code FROM currency WHERE code <> ?1 ORDER BY code")?
            .query_map([&base], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        symbols.retain(|s| s != &base);
        if symbols.is_empty() {
            return Ok((on.to_string(), 0));
        }

        let url = format!("{BASE}/{}?base={base}&symbols={}", on, symbols.join(","));
        let body = ureq::get(&url)
            .call()
            .map_err(|e| FxError::BadRate(format!("fetch failed: {e}")))?
            .body_mut()
            .read_to_string()
            .map_err(|e| FxError::BadRate(format!("unreadable response: {e}")))?;

        let doc: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| FxError::BadRate(format!("not JSON: {e}")))?;
        // THE important line: the source's date, not ours.
        let reported = doc
            .get("date")
            .and_then(|v| v.as_str())
            .ok_or_else(|| FxError::BadRate("response has no date".into()))?
            .to_string();
        let rates = doc
            .get("rates")
            .and_then(|v| v.as_object())
            .ok_or_else(|| FxError::BadRate("response has no rates".into()))?;

        let mut n = 0;
        for (code, value) in rates {
            // Serialize the number back to text and parse it exactly -- going via f64 would
            // reintroduce the imprecision the rational representation exists to avoid.
            let rate = decimal_to_rate(&value.to_string())?;
            put_rate(conn, &base, code, &reported, "ecb", rate)?;
            n += 1;
        }
        Ok((reported, n))
    }
}

#[cfg(test)]
mod fetch_tests {
    use super::fetch::decimal_to_rate;
    use super::Rate;

    #[test]
    fn decimals_become_exact_rationals() {
        assert_eq!(decimal_to_rate("1.1666").unwrap(), Rate { num: 11_666, den: 10_000 });
        assert_eq!(decimal_to_rate("216.89").unwrap(), Rate { num: 21_689, den: 100 });
        assert_eq!(decimal_to_rate("190").unwrap(), Rate { num: 190, den: 1 });
        // and the value that a float would mangle round-trips exactly
        let r = decimal_to_rate("1.005").unwrap();
        assert_eq!(r, Rate { num: 1_005, den: 1_000 });
    }

    #[test]
    fn junk_is_refused() {
        assert!(decimal_to_rate("not a rate").is_err());
        assert!(decimal_to_rate("").is_err());
    }
}
