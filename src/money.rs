//! Integer money. The ONLY place rounding is implemented.
//!
//! Amounts are signed minor units (pence, cents, satoshi) in an `i64`. There is no `f64` in this
//! file and none may be introduced in any file that consumes it -- a float that reaches an amount
//! is a bug that shows up years later as a penny that will not reconcile.
//!
//! `minor_digits` runs 0..=8. Eight is not arbitrary: BTC's native precision is 8, and `i64` at 8
//! decimals still represents 92 billion units. ETH's native 18 decimals would overflow `i64` at
//! 9.22 ETH, so ETH is held at 8 decimals -- the discarded precision is worth about £0.00002.

use std::fmt;

/// A signed amount in minor units. The currency is carried alongside, never inside.
pub type Minor = i64;

pub const MAX_MINOR_DIGITS: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoneyError {
    /// The text was not a number at all.
    NotANumber(String),
    /// More decimal places than the currency has. REFUSED rather than rounded: silently dropping a
    /// digit from imported data is how a ledger acquires a discrepancy nobody can trace.
    TooPrecise { got: u32, allowed: u32 },
    /// Outside i64 after scaling.
    Overflow(String),
    /// minor_digits outside 0..=MAX_MINOR_DIGITS.
    BadScale(u32),
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoneyError::NotANumber(s) => write!(f, "not a number: {s:?}"),
            MoneyError::TooPrecise { got, allowed } => {
                write!(f, "{got} decimal places but the currency allows {allowed}")
            }
            MoneyError::Overflow(s) => write!(f, "amount out of range: {s:?}"),
            MoneyError::BadScale(d) => write!(f, "minor_digits {d} outside 0..={MAX_MINOR_DIGITS}"),
        }
    }
}

impl std::error::Error for MoneyError {}

/// Round `num / den` to the nearest integer, halves away from zero.
///
/// Half-away-from-zero (not banker's rounding) because that is what UK consumer finance does: a
/// statement showing 0.5p of interest charges 1p. Consistency matters more than the choice -- this
/// is the single implementation, so interest, FX valuation and apportionment cannot disagree.
///
/// Takes `i128` so a caller may multiply two `i64`s before dividing without overflowing first.
/// Panics only on `den == 0`, which is a programming error, never input.
/// Not yet called from a handler -- interest and FX (build steps 5 and 6) are its consumers.
#[allow(dead_code)]
pub fn round_half_away(num: i128, den: i128) -> Result<Minor, MoneyError> {
    assert!(den != 0, "round_half_away: zero denominator");
    // Normalise the sign onto the numerator so the halfway test is symmetric.
    let (num, den) = if den < 0 { (-num, -den) } else { (num, den) };
    let q = num / den;
    let r = num % den;
    let bumped = if r == 0 {
        q
    } else if r > 0 {
        if 2 * r >= den {
            q + 1
        } else {
            q
        }
    } else if -2 * r >= den {
        q - 1
    } else {
        q
    };
    i64::try_from(bumped).map_err(|_| MoneyError::Overflow(bumped.to_string()))
}

/// Parse a human/CSV amount into minor units for a currency with `minor_digits`.
///
/// Deliberately tolerant of what real bank exports contain -- currency symbols, thousands
/// separators, non-breaking spaces, and parentheses for negatives -- and deliberately intolerant of
/// excess precision, which is a data problem rather than a formatting one.
pub fn parse_minor(text: &str, minor_digits: u32) -> Result<Minor, MoneyError> {
    if minor_digits > MAX_MINOR_DIGITS {
        return Err(MoneyError::BadScale(minor_digits));
    }
    let raw = text.trim();
    if raw.is_empty() {
        return Err(MoneyError::NotANumber(text.to_string()));
    }

    // Parentheses are the accounting negative: (12.34) is -12.34.
    let (body, paren_neg) = match raw.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        Some(inner) => (inner.trim(), true),
        None => (raw, false),
    };

    let mut digits = String::with_capacity(body.len());
    let mut seen_dot = false;
    let mut neg = false;
    let mut frac: u32 = 0;

    for (i, ch) in body.chars().enumerate() {
        match ch {
            '-' | '\u{2212}' if i == 0 => neg = true, // ASCII hyphen or Unicode minus
            '+' if i == 0 => {}
            '0'..='9' => {
                digits.push(ch);
                if seen_dot {
                    frac += 1;
                }
            }
            '.' if !seen_dot => seen_dot = true,
            // Separators and symbols that carry no value. ',' is treated as a thousands separator:
            // this is a UK tool and a comma decimal point would be ambiguous with it, so a European
            // "1,50" is refused by the precision check rather than silently read as 150.
            ',' | ' ' | '\u{00a0}' | '\u{202f}' | '_' => {}
            '£' | '$' | '€' | '¥' => {}
            _ => return Err(MoneyError::NotANumber(text.to_string())),
        }
    }

    if digits.is_empty() {
        return Err(MoneyError::NotANumber(text.to_string()));
    }
    if frac > minor_digits {
        return Err(MoneyError::TooPrecise { got: frac, allowed: minor_digits });
    }

    let mut value: i128 = digits
        .parse::<i128>()
        .map_err(|_| MoneyError::Overflow(text.to_string()))?;
    // Scale up by the digits the input did not supply.
    for _ in 0..(minor_digits - frac) {
        value = value
            .checked_mul(10)
            .ok_or_else(|| MoneyError::Overflow(text.to_string()))?;
    }
    if neg || paren_neg {
        value = -value;
    }
    i64::try_from(value).map_err(|_| MoneyError::Overflow(text.to_string()))
}

/// Render minor units as a plain decimal string. No symbol, no thousands separators, always
/// exactly `minor_digits` places -- this is for round-tripping and export, not for display chrome.
pub fn format_minor(amount: Minor, minor_digits: u32) -> String {
    if minor_digits == 0 {
        return amount.to_string();
    }
    let neg = amount < 0;
    // Via i128: (-i64::MIN) overflows i64.
    let abs = (amount as i128).unsigned_abs();
    let scale = 10u128.pow(minor_digits);
    let whole = abs / scale;
    let frac = abs % scale;
    format!(
        "{}{}.{:0width$}",
        if neg { "-" } else { "" },
        whole,
        frac,
        width = minor_digits as usize
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_real_exports_contain() {
        assert_eq!(parse_minor("12.34", 2).unwrap(), 1234);
        assert_eq!(parse_minor("  12.34  ", 2).unwrap(), 1234);
        assert_eq!(parse_minor("£12.34", 2).unwrap(), 1234);
        assert_eq!(parse_minor("1,234.56", 2).unwrap(), 123456);
        assert_eq!(parse_minor("1\u{00a0}234.56", 2).unwrap(), 123456); // NBSP thousands
        assert_eq!(parse_minor("-12.34", 2).unwrap(), -1234);
        assert_eq!(parse_minor("\u{2212}12.34", 2).unwrap(), -1234); // Unicode minus
        assert_eq!(parse_minor("(12.34)", 2).unwrap(), -1234); // accounting negative
        assert_eq!(parse_minor("12", 2).unwrap(), 1200); // implied places
        assert_eq!(parse_minor("12.3", 2).unwrap(), 1230);
        assert_eq!(parse_minor("0", 2).unwrap(), 0);
        assert_eq!(parse_minor("-0.01", 2).unwrap(), -1);
    }

    #[test]
    fn refuses_rather_than_rounds_excess_precision() {
        // The important one: silently dropping a digit is how a ledger acquires an untraceable
        // discrepancy. 1.005 in a 2dp currency is a data problem, not a formatting one.
        assert_eq!(
            parse_minor("1.005", 2),
            Err(MoneyError::TooPrecise { got: 3, allowed: 2 })
        );
        assert!(parse_minor("0.123456789", 8).is_err()); // 9dp, past BTC's precision
    }

    #[test]
    fn refuses_junk_and_blank() {
        assert!(parse_minor("", 2).is_err());
        assert!(parse_minor("   ", 2).is_err());
        assert!(parse_minor("n/a", 2).is_err());
        assert!(parse_minor("12.3.4", 2).is_err()); // second dot becomes junk
        assert!(parse_minor("£", 2).is_err()); // symbol but no digits
        assert!(parse_minor("1.0", 9).is_err()); // scale past MAX_MINOR_DIGITS
    }

    #[test]
    fn crypto_scales_fit() {
        // BTC at its native precision.
        assert_eq!(parse_minor("0.00000001", 8).unwrap(), 1);
        assert_eq!(parse_minor("21000000", 8).unwrap(), 2_100_000_000_000_000);
        // and the value that would have overflowed at 18dp is comfortable at 8.
        assert_eq!(parse_minor("9.22337203", 8).unwrap(), 922_337_203);
    }

    #[test]
    fn rounds_halves_away_from_zero_symmetrically() {
        assert_eq!(round_half_away(5, 2).unwrap(), 3); // 2.5 -> 3
        assert_eq!(round_half_away(-5, 2).unwrap(), -3); // -2.5 -> -3, not -2
        assert_eq!(round_half_away(4, 2).unwrap(), 2);
        assert_eq!(round_half_away(1, 3).unwrap(), 0);
        assert_eq!(round_half_away(2, 3).unwrap(), 1);
        assert_eq!(round_half_away(-2, 3).unwrap(), -1);
        assert_eq!(round_half_away(0, 7).unwrap(), 0);
        // a negative denominator must not flip the halfway rule
        assert_eq!(round_half_away(5, -2).unwrap(), -3);
    }

    #[test]
    fn interest_arithmetic_is_exact() {
        // £50m at 24.99% APR, one twelfth. Realistic magnitudes do NOT overflow i64 here
        // (5e9 * 2499 is about 1.2e13) -- the i128 signature is headroom, not a live necessity
        // for interest. See the FX test below for the case where it actually bites.
        let balance: i128 = 5_000_000_000;
        let apr_bp: i128 = 2_499;
        assert_eq!(round_half_away(balance * apr_bp, 10_000 * 12).unwrap(), 104_125_000);
    }

    #[test]
    fn rounding_survives_a_product_that_overflows_i64() {
        // Where i128 genuinely earns its place: FX rates are stored as exact rationals, so a
        // conversion multiplies an amount by a numerator that can be large. An i64 intermediate
        // would wrap silently and produce a plausible wrong number rather than an error.
        let amount: i128 = 10_000_000_000; // £100m in pence
        let num: i128 = 2_406_412_345; // 1 GBP = 24064.12345 IDR, as num/den
        let den: i128 = 100_000;
        assert!(amount * num > i64::MAX as i128, "the product must exceed i64 for this to prove anything");
        let got = round_half_away(amount * num, den).unwrap();
        assert_eq!(got, 240_641_234_500_000);
        // and the result itself still fits, which is what makes returning i64 sound here
        assert!(got < i64::MAX);
    }

    #[test]
    fn formats_and_round_trips() {
        assert_eq!(format_minor(1234, 2), "12.34");
        assert_eq!(format_minor(-1234, 2), "-12.34");
        assert_eq!(format_minor(5, 2), "0.05");
        assert_eq!(format_minor(-5, 2), "-0.05");
        assert_eq!(format_minor(0, 2), "0.00");
        assert_eq!(format_minor(1, 8), "0.00000001");
        assert_eq!(format_minor(100, 0), "100");
        for (v, d) in [(1234i64, 2u32), (-1i64, 2), (0, 2), (1, 8), (-999_999, 8)] {
            assert_eq!(parse_minor(&format_minor(v, d), d).unwrap(), v, "round trip {v}@{d}");
        }
    }

    #[test]
    fn format_handles_i64_min_without_overflowing() {
        // -(i64::MIN) is not representable in i64; the abs must go via i128.
        let s = format_minor(i64::MIN, 2);
        assert!(s.starts_with("-92233720368547758.0"), "got {s}");
    }
}
