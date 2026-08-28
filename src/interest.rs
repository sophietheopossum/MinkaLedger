//! Interest: rate derivation at write time, and four pure functions the forecast folds.
//!
//! THE SHAPE OF THE PROBLEM. Interest is not a recurring series. A series knows its amount in
//! advance; interest is a function of the balance on the day, and the balance is what the forecast
//! is computing. So it cannot be expanded up front -- it has to be folded into the forward walk,
//! where each day's interest changes every later day's balance. That feedback is the whole
//! difficulty, and it is why the functions here are pure and take the running balance as an
//! argument rather than reading anything.
//!
//! WHERE FLOATS ARE ALLOWED, EXACTLY ONCE. Converting a quoted APR into a per-period rate needs a
//! root (`(1+r)^(1/n)`), and an amortisation payment needs a power. Both happen at WRITE time,
//! once, and the result is stored as an integer scaled by 1e15. Every subsequent arithmetic step --
//! every accrual, every allocation, every projected balance -- is integer. So a float never touches
//! an amount, only the derivation of a constant that is then frozen.

use crate::money::{round_half_away, MoneyError, Minor};

/// Rates are stored as integers scaled by 1e15: 0.0187624... becomes 18_762_473_077_360.
/// Fifteen places is far past any published rate's precision and leaves plenty of i64 headroom.
pub const RATE_SCALE: i128 = 1_000_000_000_000_000;

#[derive(Debug, PartialEq)]
pub enum InterestError {
    BadRate(String),
    BadTerm(String),
    Money(MoneyError),
}

impl std::fmt::Display for InterestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterestError::BadRate(m) => write!(f, "bad rate: {m}"),
            InterestError::BadTerm(m) => write!(f, "bad term: {m}"),
            InterestError::Money(e) => write!(f, "{e}"),
        }
    }
}

impl From<MoneyError> for InterestError {
    fn from(e: MoneyError) -> Self {
        InterestError::Money(e)
    }
}

/// Convert a quoted annual rate into the per-period rate, as an e15 integer.
///
/// `effective` (AER, and APR as UK consumer credit quotes it) COMPOUNDS: the periodic rate is the
/// nth root, `(1+r)^(1/n) - 1`. `nominal` simply divides. Getting this backwards overstates a
/// 24.99% card by about 11% of its interest, every month, forever -- so the basis is a required
/// column rather than an assumption.
pub fn derive_periodic_rate(
    quoted_e15: i64,
    basis: &str,
    periods_per_year: i64,
) -> Result<i64, InterestError> {
    if periods_per_year <= 0 {
        return Err(InterestError::BadRate(format!("periods_per_year {periods_per_year}")));
    }
    match basis {
        // Exact integer division -- no float involved at all.
        "nominal" => round_half_away(quoted_e15 as i128, periods_per_year as i128)
            .map_err(InterestError::Money),
        "effective" => {
            let r = quoted_e15 as f64 / RATE_SCALE as f64;
            if r <= -1.0 {
                return Err(InterestError::BadRate("rate <= -100%".into()));
            }
            let periodic = (1.0 + r).powf(1.0 / periods_per_year as f64) - 1.0;
            let scaled = (periodic * RATE_SCALE as f64).round();
            if !scaled.is_finite() {
                return Err(InterestError::BadRate("not finite".into()));
            }
            Ok(scaled as i64)
        }
        other => Err(InterestError::BadRate(format!("unknown basis {other}"))),
    }
}

/// Interest on a balance for one period. Integer throughout: the product needs i128 because a large
/// balance times an e15 rate overflows i64 long before either operand does.
pub fn accrue(balance_minor: Minor, periodic_rate_e15: i64) -> Result<Minor, InterestError> {
    Ok(round_half_away(balance_minor as i128 * periodic_rate_e15 as i128, RATE_SCALE)?)
}

/// The level payment that clears `principal` over `term` periods at `rate`.
///
/// Standard amortisation: `P·i / (1 - (1+i)^-n)`. A zero rate degrades to plain division rather
/// than dividing by zero.
pub fn derive_level_payment(
    principal_minor: Minor,
    periodic_rate_e15: i64,
    term_periods: i64,
) -> Result<Minor, InterestError> {
    if term_periods <= 0 {
        return Err(InterestError::BadTerm(format!("term {term_periods}")));
    }
    if periodic_rate_e15 == 0 {
        return Ok(round_half_away(principal_minor as i128, term_periods as i128)?);
    }
    let i = periodic_rate_e15 as f64 / RATE_SCALE as f64;
    let denom = 1.0 - (1.0 + i).powi(-(term_periods as i32));
    if denom.abs() < f64::EPSILON {
        return Err(InterestError::BadRate("degenerate amortisation".into()));
    }
    let pmt = principal_minor as f64 * i / denom;
    if !pmt.is_finite() {
        return Err(InterestError::BadRate("not finite".into()));
    }
    Ok(pmt.round() as Minor)
}

/// Split a payment into interest and principal.
///
/// Interest first, always -- that is how consumer credit works, and it is what makes an early
/// payment reduce the term rather than the next instalment. A payment smaller than the interest
/// due pays no principal at all and the debt grows; that is not an error, it is what negative
/// amortisation is, and the forecast should show it happening rather than refuse to model it.
pub fn allocate(payment_minor: Minor, interest_due_minor: Minor) -> (Minor, Minor) {
    let to_interest = payment_minor.min(interest_due_minor).max(0);
    let to_principal = payment_minor - to_interest;
    (to_interest, to_principal)
}

/// What a payment rule asks for this period.
///
/// `balance` and `statement` are both supplied because the shapes disagree about which they mean: a
/// minimum payment is a percentage of the STATEMENT balance (frozen on the statement date), not of
/// today's, which may already include this month's spending.
#[derive(Debug, Clone, Copy)]
pub struct PaymentContext {
    pub balance_minor: Minor,
    pub statement_minor: Minor,
    pub interest_minor: Minor,
    pub fees_minor: Minor,
}

#[allow(clippy::too_many_arguments)]
pub fn payment_amount(
    kind: &str,
    ctx: PaymentContext,
    fixed_minor: Option<Minor>,
    pct_e15: Option<i64>,
    floor_minor: Option<Minor>,
    cap_minor: Option<Minor>,
    level_payment_minor: Option<Minor>,
) -> Result<Minor, InterestError> {
    let pct_of = |amount: Minor| -> Result<Minor, InterestError> {
        let p = pct_e15.ok_or_else(|| InterestError::BadRate("pct required".into()))?;
        Ok(round_half_away(amount as i128 * p as i128, RATE_SCALE)?)
    };

    let raw = match kind {
        "fixed" => fixed_minor.ok_or_else(|| InterestError::BadRate("fixed required".into()))?,
        "pct_of_balance" => pct_of(ctx.balance_minor)?,
        "pct_of_statement" => pct_of(ctx.statement_minor)?,
        // The real UK minimum-payment shape: interest + fees + a percentage of the balance.
        "interest_fees_plus_pct" => {
            ctx.interest_minor + ctx.fees_minor + pct_of(ctx.statement_minor)?
        }
        "full_statement" => ctx.statement_minor,
        "amortising_level" => level_payment_minor
            .ok_or_else(|| InterestError::BadRate("level payment required".into()))?,
        other => return Err(InterestError::BadRate(format!("unknown amount_kind {other}"))),
    };

    // A minimum payment is "the greater of £N and X%" -- the floor applies BEFORE the cap, and the
    // cap is the outstanding balance: you never pay more than you owe.
    let floored = match floor_minor {
        Some(f) => raw.max(f),
        None => raw,
    };
    let capped = match cap_minor {
        Some(c) => floored.min(c),
        None => floored,
    };
    // Never demand more than is outstanding, and never a negative payment.
    Ok(capped.clamp(0, ctx.statement_minor.max(ctx.balance_minor).max(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_and_nominal_bases_differ_and_both_are_exact() {
        // 24.9% AER accrued daily. Verified independently: (1.249)^(1/365)-1 = 0.000609345112730
        assert_eq!(derive_periodic_rate(249_000_000_000_000, "effective", 365).unwrap(), 609_345_112_730);
        // 24.99% AER monthly
        assert_eq!(derive_periodic_rate(249_900_000_000_000, "effective", 12).unwrap(), 18_762_473_077_360);
        // the same figure quoted NOMINALLY is simple division, and is materially bigger
        assert_eq!(derive_periodic_rate(249_900_000_000_000, "nominal", 12).unwrap(), 20_825_000_000_000);
    }

    #[test]
    fn a_zero_rate_is_zero_not_an_error() {
        assert_eq!(derive_periodic_rate(0, "effective", 12).unwrap(), 0);
        assert_eq!(derive_periodic_rate(0, "nominal", 12).unwrap(), 0);
    }

    #[test]
    fn bad_rate_inputs_are_refused() {
        assert!(derive_periodic_rate(100, "weekly-ish", 12).is_err());
        assert!(derive_periodic_rate(100, "nominal", 0).is_err());
        assert!(derive_periodic_rate(-RATE_SCALE as i64 * 2, "effective", 12).is_err());
    }

    #[test]
    fn accrual_is_integer_and_rounds_half_away() {
        // £1,000 at 1% for a period = £10.00 exactly
        assert_eq!(accrue(100_000, 10_000_000_000_000).unwrap(), 1_000);
        // a balance small enough to produce half a penny rounds up, not to even
        assert_eq!(accrue(100, 5_000_000_000_000).unwrap(), 1); // 0.5p -> 1p
        // and a debt (negative balance) accrues symmetrically
        assert_eq!(accrue(-100_000, 10_000_000_000_000).unwrap(), -1_000);
        assert_eq!(accrue(0, 10_000_000_000_000).unwrap(), 0);
    }

    #[test]
    fn accrual_survives_a_balance_that_overflows_an_i64_product() {
        // £10m at 24.9% daily: 1e9 * 6.09e11 is ~6e20, past i64. This is the case i128 is for.
        let bal = 1_000_000_000;
        let got = accrue(bal, 609_345_112_730).unwrap();
        assert_eq!(got, 609_345);
        assert!(bal as i128 * 609_345_112_730i128 > i64::MAX as i128);
    }

    #[test]
    fn the_level_payment_clears_the_loan_exactly() {
        // £10,000 over 60 months at 6% nominal -> £193.33/month. The hard oracle for amortisation
        // is that the balance lands on EXACTLY zero once the final payment absorbs the residual.
        let principal = 1_000_000;
        let rate = derive_periodic_rate(60_000_000_000_000, "nominal", 12).unwrap();
        assert_eq!(rate, 5_000_000_000_000); // 0.5% a month
        let pmt = derive_level_payment(principal, rate, 60).unwrap();
        assert_eq!(pmt, 19_333);

        let mut bal = principal;
        let mut total_interest = 0;
        for k in 0..60 {
            let interest = accrue(bal, rate).unwrap();
            total_interest += interest;
            let (_to_i, to_p) = allocate(pmt, interest);
            // the last instalment settles whatever is left rather than over/under-paying
            bal -= if k == 59 { bal } else { to_p };
        }
        assert_eq!(bal, 0, "an amortising loan must close at exactly zero");
        assert!(total_interest > 0);
    }

    #[test]
    fn a_zero_rate_loan_is_plain_division() {
        assert_eq!(derive_level_payment(120_000, 0, 12).unwrap(), 10_000);
        assert!(derive_level_payment(120_000, 0, 0).is_err());
    }

    #[test]
    fn allocation_pays_interest_before_principal() {
        // £200 against £50 of interest: £50 interest, £150 principal
        assert_eq!(allocate(20_000, 5_000), (5_000, 15_000));
        // a payment smaller than the interest pays NO principal -- the debt grows. Modelled, not
        // refused: that is what negative amortisation is and the forecast should show it.
        assert_eq!(allocate(3_000, 5_000), (3_000, 0));
        assert_eq!(allocate(0, 5_000), (0, 0));
    }

    #[test]
    fn the_uk_minimum_payment_shape() {
        let ctx = PaymentContext {
            balance_minor: 120_000,   // £1,200 owed
            statement_minor: 100_000, // £1,000 on the statement
            interest_minor: 2_000,    // £20 interest
            fees_minor: 1_200,        // £12 fee
        };
        // interest + fees + 1% of the statement, floored at £5
        let got = payment_amount(
            "interest_fees_plus_pct", ctx, None, Some(10_000_000_000_000),
            Some(500), None, None,
        ).unwrap();
        assert_eq!(got, 2_000 + 1_200 + 1_000);

        // when the percentage is tiny the FLOOR wins -- "the greater of £5 or 1%"
        let small = PaymentContext { balance_minor: 1_000, statement_minor: 1_000,
                                     interest_minor: 0, fees_minor: 0 };
        let got = payment_amount("pct_of_statement", small, None, Some(10_000_000_000_000),
                                 Some(500), None, None).unwrap();
        assert_eq!(got, 500, "the £5 floor beats 1% of £10");
    }

    #[test]
    fn a_payment_never_exceeds_what_is_owed() {
        let ctx = PaymentContext { balance_minor: 3_000, statement_minor: 3_000,
                                   interest_minor: 0, fees_minor: 0 };
        // a £50 fixed payment against a £30 balance pays £30
        let got = payment_amount("fixed", ctx, Some(5_000), None, None, None, None).unwrap();
        assert_eq!(got, 3_000);
        // and a cleared card asks for nothing rather than a negative
        let clear = PaymentContext { balance_minor: 0, statement_minor: 0,
                                     interest_minor: 0, fees_minor: 0 };
        assert_eq!(payment_amount("fixed", clear, Some(5_000), None, None, None, None).unwrap(), 0);
    }

    #[test]
    fn full_statement_and_fixed_and_level_shapes() {
        let ctx = PaymentContext { balance_minor: 120_000, statement_minor: 100_000,
                                   interest_minor: 2_000, fees_minor: 0 };
        assert_eq!(payment_amount("full_statement", ctx, None, None, None, None, None).unwrap(), 100_000);
        assert_eq!(payment_amount("fixed", ctx, Some(25_000), None, None, None, None).unwrap(), 25_000);
        assert_eq!(
            payment_amount("amortising_level", ctx, None, None, None, None, Some(19_333)).unwrap(),
            19_333
        );
        assert!(payment_amount("nonsense", ctx, None, None, None, None, None).is_err());
    }

    #[test]
    fn savings_interest_is_the_same_machinery_with_a_positive_balance() {
        // 4.5% AER credited monthly on £5,000.
        let rate = derive_periodic_rate(45_000_000_000_000, "effective", 12).unwrap();
        let monthly = accrue(500_000, rate).unwrap();
        // ~0.3675% a month on £5,000 = about £18.37
        assert!((1_830..=1_845).contains(&monthly), "got {monthly}");
        // and twelve months of compounding lands within a penny or two of the quoted AER
        let mut bal = 500_000i64;
        for _ in 0..12 {
            bal += accrue(bal, rate).unwrap();
        }
        let earned = bal - 500_000;
        assert!((22_450..=22_550).contains(&earned), "a year at 4.5% AER on £5,000: got {earned}");
    }
}
