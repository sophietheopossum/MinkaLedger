//! Recurrence: turning an RFC 5545 rule into the dates it lands on.
//!
//! Behind a trait so the `rrule` crate is swappable. That is not speculative generality -- rrule
//! 0.14.0 was last released in April 2025 and expansion correctness is the one thing in this
//! program that absolutely cannot be quietly wrong, so being able to substitute an implementation
//! and run the same corpus against it is worth the forty lines.
//!
//! DATES ONLY. A ledger occurrence is a date, never an instant: UK bank exports are date-granular,
//! so any time we stored would be invented. To keep the recurrence library's timezone machinery
//! from ever changing a date under us, every date crosses into it as **noon UTC** and comes back
//! via `date_naive()`. Noon is the standard trick -- a DST shift of ±1h cannot move noon across a
//! midnight boundary, so a rule that says "the 28th" yields the 28th in every zone and season.

use chrono::{Datelike, NaiveDate, TimeZone};
use rrule::{RRuleSet, Tz};

#[derive(Debug)]
pub enum RecurError {
    /// The rule text did not parse, with the library's reason.
    BadRule(String),
    BadDate(String),
}

impl std::fmt::Display for RecurError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecurError::BadRule(m) => write!(f, "bad recurrence rule: {m}"),
            RecurError::BadDate(m) => write!(f, "bad date: {m}"),
        }
    }
}

/// Expansion, isolated so it can be swapped or fuzzed against a second implementation.
pub trait Recurrence {
    /// Every occurrence of `rrule` (an RFC 5545 RRULE body, no DTSTART line) starting at `dtstart`,
    /// within `lo..=hi` inclusive. `until` bounds the series itself.
    fn expand(
        &self,
        rrule: &str,
        dtstart: NaiveDate,
        until: Option<NaiveDate>,
        lo: NaiveDate,
        hi: NaiveDate,
    ) -> Result<Vec<NaiveDate>, RecurError>;
}

/// Hard ceiling on occurrences per expansion. A daily rule over a 30-year horizon is ~11k, so this
/// is generous; it exists because `RRuleSet::all` demands a limit and an unbounded rule would
/// otherwise be a hang rather than an error.
const EXPANSION_LIMIT: u16 = u16::MAX;

pub fn utc_noon(d: NaiveDate) -> chrono::DateTime<Tz> {
    Tz::UTC
        .with_ymd_and_hms(d.year(), d.month(), d.day(), 12, 0, 0)
        .single()
        .expect("noon UTC is unambiguous on every date")
}

pub struct RRuleCrate;

impl Recurrence for RRuleCrate {
    fn expand(
        &self,
        rrule: &str,
        dtstart: NaiveDate,
        until: Option<NaiveDate>,
        lo: NaiveDate,
        hi: NaiveDate,
    ) -> Result<Vec<NaiveDate>, RecurError> {
        if hi < lo {
            return Ok(Vec::new());
        }
        // UNTIL is applied by us rather than folded into the rule text: RFC 5545 UNTIL is an
        // instant, and appending one to a rule the operator wrote risks contradicting a UNTIL
        // already in it. Our until_on is an INCLUSIVE date (RFC 5545 3.3.10), so it is a filter.
        let effective_hi = match until {
            Some(u) if u < hi => u,
            _ => hi,
        };
        if effective_hi < dtstart || effective_hi < lo {
            return Ok(Vec::new());
        }

        let text = format!(
            "DTSTART:{}\nRRULE:{}",
            utc_noon(dtstart).format("%Y%m%dT%H%M%SZ"),
            rrule.trim()
        );
        let set: RRuleSet = text.parse().map_err(|e| RecurError::BadRule(format!("{e}")))?;

        // Bound the expansion by the horizon, or a DAILY rule generates its full 65535-occurrence
        // limit (179 years) to keep a handful of dates.
        //
        // `before()`'s inclusivity is undocumented, so we ask for one day PAST the horizon and then
        // filter exactly ourselves. Over-fetching by a day makes the semantics irrelevant: whether
        // the crate treats the bound as inclusive or exclusive, our own filter decides, and the
        // horizon-boundary test below pins that.
        let fetch_to = effective_hi.succ_opt().unwrap_or(effective_hi);
        let result = set.before(utc_noon(fetch_to)).all(EXPANSION_LIMIT);
        let mut out: Vec<NaiveDate> = result
            .dates
            .into_iter()
            .map(|dt| dt.date_naive())
            .filter(|d| *d >= lo && *d <= effective_hi)
            .collect();
        out.dedup();
        Ok(out)
    }
}

/// Move a date off a weekend, per the series' `weekend_rule`.
///
/// This is applied AFTER expansion and moves the value date only -- never the occurrence identity.
/// That distinction is load-bearing: the slot a real transaction claims is the unadjusted date, so
/// changing the weekend rule later cannot orphan existing overrides or double-project a payment.
pub fn business_adjust(d: NaiveDate, rule: &str, holidays: &[NaiveDate]) -> NaiveDate {
    let blocked = |x: &NaiveDate| {
        matches!(x.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) || holidays.contains(x)
    };
    if rule == "none" || !blocked(&d) {
        return d;
    }
    let step = |x: NaiveDate, forward: bool| {
        let mut c = x;
        // A bounded walk: 10 is more than any run of weekend + bank holidays.
        for _ in 0..10 {
            c = if forward { c.succ_opt().unwrap_or(c) } else { c.pred_opt().unwrap_or(c) };
            if !blocked(&c) {
                return c;
            }
        }
        c
    };
    match rule {
        "before" => step(d, false),
        "after" => step(d, true),
        // "modified following": forward, unless that crosses into the next month, then backward.
        // The convention UK direct debits actually use.
        "modified_after" => {
            let fwd = step(d, true);
            if fwd.month() != d.month() {
                step(d, false)
            } else {
                fwd
            }
        }
        _ => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn expand(rule: &str, start: &str, lo: &str, hi: &str) -> Vec<String> {
        RRuleCrate
            .expand(rule, d(start), None, d(lo), d(hi))
            .unwrap()
            .iter()
            .map(|x| x.to_string())
            .collect()
    }

    #[test]
    fn monthly_on_a_fixed_day() {
        let got = expand("FREQ=MONTHLY;BYMONTHDAY=1", "2026-01-01", "2026-01-01", "2026-04-30");
        assert_eq!(got, ["2026-01-01", "2026-02-01", "2026-03-01", "2026-04-01"]);
    }

    #[test]
    fn the_uk_payday_rule_last_working_day_of_the_month() {
        // The single most common UK salary rule, and the one most likely to be got wrong.
        let got = expand(
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
            "2026-01-01",
            "2026-01-01",
            "2026-06-30",
        );
        // Jan 30 is a Friday (31st is Sat); Feb 27 Fri (28th Sat); Mar 31 Tue; Apr 30 Thu;
        // May 29 Fri (30/31 weekend); Jun 30 Tue.
        assert_eq!(
            got,
            ["2026-01-30", "2026-02-27", "2026-03-31", "2026-04-30", "2026-05-29", "2026-06-30"]
        );
    }

    #[test]
    fn bymonthday_31_skips_short_months_rather_than_clamping() {
        // RFC 5545 says an invalid date is OMITTED, not clamped to the 30th. A ledger that clamps
        // would invent a payment in February that never happens.
        let got = expand("FREQ=MONTHLY;BYMONTHDAY=31", "2026-01-31", "2026-01-01", "2026-08-31");
        assert_eq!(got, ["2026-01-31", "2026-03-31", "2026-05-31", "2026-07-31", "2026-08-31"]);
    }

    #[test]
    fn dst_transitions_do_not_move_the_date() {
        // Europe/London springs forward 2026-03-29 and falls back 2026-10-25. Noon-UTC anchoring
        // is what keeps a "29th of the month" rule landing on the 29th through both.
        let spring = expand("FREQ=MONTHLY;BYMONTHDAY=29", "2026-01-29", "2026-03-01", "2026-03-31");
        assert_eq!(spring, ["2026-03-29"]);
        let autumn = expand("FREQ=MONTHLY;BYMONTHDAY=25", "2026-01-25", "2026-10-01", "2026-10-31");
        assert_eq!(autumn, ["2026-10-25"]);
        // and daily across the spring-forward weekend loses no day and repeats none
        let daily = expand("FREQ=DAILY", "2026-03-27", "2026-03-27", "2026-03-31");
        assert_eq!(daily, ["2026-03-27", "2026-03-28", "2026-03-29", "2026-03-30", "2026-03-31"]);
    }

    #[test]
    fn horizon_bounds_are_inclusive_at_both_ends() {
        // Pinned deliberately: the crate's before()/after() inclusivity is undocumented, so we
        // filter ourselves. If that ever changes, this fails rather than silently dropping a
        // payment on the horizon edge.
        let got = expand("FREQ=MONTHLY;BYMONTHDAY=15", "2026-01-15", "2026-01-15", "2026-03-15");
        assert_eq!(got, ["2026-01-15", "2026-02-15", "2026-03-15"]);
    }

    #[test]
    fn until_bounds_the_series_inclusively() {
        let got = RRuleCrate
            .expand(
                "FREQ=MONTHLY;BYMONTHDAY=1",
                d("2026-01-01"),
                Some(d("2026-03-01")),
                d("2026-01-01"),
                d("2026-12-31"),
            )
            .unwrap();
        assert_eq!(got.len(), 3, "until_on is inclusive (RFC 5545 3.3.10): {got:?}");
        assert_eq!(got.last().unwrap().to_string(), "2026-03-01");
    }

    #[test]
    fn weekly_and_fortnightly() {
        let weekly = expand("FREQ=WEEKLY;BYDAY=FR", "2026-08-07", "2026-08-01", "2026-08-31");
        assert_eq!(weekly, ["2026-08-07", "2026-08-14", "2026-08-21", "2026-08-28"]);
        let fortnightly = expand("FREQ=WEEKLY;INTERVAL=2;BYDAY=FR", "2026-08-07", "2026-08-01", "2026-09-30");
        assert_eq!(fortnightly, ["2026-08-07", "2026-08-21", "2026-09-04", "2026-09-18"]);
    }

    #[test]
    fn an_empty_or_reversed_window_yields_nothing() {
        assert!(expand("FREQ=DAILY", "2026-01-01", "2026-03-02", "2026-03-01").is_empty());
        // window entirely before the series starts
        assert!(expand("FREQ=DAILY", "2026-06-01", "2026-01-01", "2026-02-01").is_empty());
    }

    #[test]
    fn a_long_horizon_on_a_daily_rule_stays_quick() {
        // The case that motivated bounding the expansion: without before(), this generates the
        // full 65535-occurrence limit regardless of the window asked for.
        let t = std::time::Instant::now();
        let got = expand("FREQ=DAILY", "2000-01-01", "2026-08-01", "2026-08-31");
        assert_eq!(got.len(), 31);
        assert!(t.elapsed().as_millis() < 500, "took {:?}", t.elapsed());
    }

    #[test]
    fn a_malformed_rule_is_an_error_not_a_panic() {
        assert!(RRuleCrate
            .expand("FREQ=NONSENSE", d("2026-01-01"), None, d("2026-01-01"), d("2026-12-31"))
            .is_err());
    }

    #[test]
    fn weekend_adjustment_moves_the_value_date_only() {
        let sat = d("2026-08-01"); // Saturday
        assert_eq!(business_adjust(sat, "none", &[]).to_string(), "2026-08-01");
        assert_eq!(business_adjust(sat, "before", &[]).to_string(), "2026-07-31"); // Fri
        assert_eq!(business_adjust(sat, "after", &[]).to_string(), "2026-08-03"); // Mon
        // a weekday is untouched by every rule
        let wed = d("2026-08-05");
        for r in ["none", "before", "after", "modified_after"] {
            assert_eq!(business_adjust(wed, r, &[]), wed);
        }
    }

    #[test]
    fn modified_following_stays_inside_the_month() {
        // 2026-08-30 is a Sunday; forward would be Mon 31 Aug -- same month, so forward.
        assert_eq!(business_adjust(d("2026-08-30"), "modified_after", &[]).to_string(), "2026-08-31");
        // 2026-05-31 is a Sunday; forward is 1 Jun, a new month, so it must go backward instead.
        assert_eq!(business_adjust(d("2026-05-31"), "modified_after", &[]).to_string(), "2026-05-29");
    }

    #[test]
    fn bank_holidays_are_skipped_like_weekends() {
        // Mon 2026-08-31 is the UK summer bank holiday.
        let hols = [d("2026-08-31")];
        assert_eq!(business_adjust(d("2026-08-30"), "after", &hols).to_string(), "2026-09-01");
        assert_eq!(business_adjust(d("2026-08-31"), "before", &hols).to_string(), "2026-08-28");
    }
}
