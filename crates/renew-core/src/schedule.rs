use crate::error::{Error, Result};
use std::time::Duration;
use time::OffsetDateTime;

/// A parsed schedule: either a fixed interval or a wall-clock cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Interval(Duration),
    Cron(Cron),
}

impl Schedule {
    /// Parse a schedule string. Accepts:
    ///   - `@every <dur>`         e.g. `@every 90s`, `@every 3m`, `@every 1h30m`
    ///   - `@hourly` `@daily` `@midnight` `@weekly` `@monthly` `@yearly` `@annually`
    ///   - a 5-field cron expression: `minute hour day-of-month month day-of-week`
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::Schedule("empty schedule".into()));
        }

        if let Some(rest) = s.strip_prefix("@every") {
            let dur = parse_duration(rest.trim())?;
            if dur.is_zero() {
                return Err(Error::Schedule("@every interval must be > 0".into()));
            }
            return Ok(Schedule::Interval(dur));
        }

        let normalized = match s {
            "@hourly" => Some("0 * * * *"),
            "@daily" | "@midnight" => Some("0 0 * * *"),
            "@weekly" => Some("0 0 * * 0"),
            "@monthly" => Some("0 0 1 * *"),
            "@yearly" | "@annually" => Some("0 0 1 1 *"),
            _ => None,
        };
        let expr = normalized.unwrap_or(s);
        Ok(Schedule::Cron(Cron::parse(expr)?))
    }

    /// The first fire strictly after `after`.
    pub fn next_after(&self, after: OffsetDateTime) -> Option<OffsetDateTime> {
        match self {
            Schedule::Interval(d) => {
                let secs = i64::try_from(d.as_secs()).ok()?;
                after
                    .checked_add(time::Duration::seconds(secs))?
                    .checked_add(time::Duration::nanoseconds(d.subsec_nanos() as i64))
            }
            Schedule::Cron(c) => c.next_after(after),
        }
    }
}

/// Parse a humantime-ish duration: a sequence of `<int><unit>` chunks where unit
/// is one of `ms`, `s`, `m`, `h`, `d`. e.g. `90s`, `3m`, `1h30m`, `500ms`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Schedule("empty duration".into()));
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total = Duration::ZERO;
    let mut saw_any = false;

    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return Err(Error::Schedule(format!(
                "expected number in duration `{s}`"
            )));
        }
        let value: u64 = s[start..i]
            .parse()
            .map_err(|_| Error::Schedule(format!("number too large in `{s}`")))?;

        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let chunk = match unit {
            "ms" => Duration::from_millis(value),
            "s" => Duration::from_secs(value),
            "m" => Duration::from_secs(value.saturating_mul(60)),
            "h" => Duration::from_secs(value.saturating_mul(3600)),
            "d" => Duration::from_secs(value.saturating_mul(86_400)),
            "" => return Err(Error::Schedule(format!("missing unit in duration `{s}`"))),
            other => {
                return Err(Error::Schedule(format!(
                    "unknown duration unit `{other}` in `{s}`"
                )))
            }
        };
        total = total
            .checked_add(chunk)
            .ok_or_else(|| Error::Schedule(format!("duration overflow in `{s}`")))?;
        saw_any = true;
    }

    if !saw_any {
        return Err(Error::Schedule(format!("could not parse duration `{s}`")));
    }
    Ok(total)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    allowed: Vec<u32>,
    wildcard: bool,
}

impl Field {
    fn matches(&self, v: u32) -> bool {
        self.allowed.binary_search(&v).is_ok()
    }
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(Error::Schedule(format!(
                "cron expression must have 5 fields (got {}): `{expr}`",
                parts.len()
            )));
        }
        let cron = Cron {
            minute: parse_field(parts[0], 0, 59, &[])?,
            hour: parse_field(parts[1], 0, 23, &[])?,
            dom: parse_field(parts[2], 1, 31, &[])?,
            month: parse_field(parts[3], 1, 12, MONTHS)?,
            dow: parse_dow(parts[4])?,
        };
        // Reject day/month combinations that can never occur (e.g. Feb 30, Apr 31).
        // Only when day-of-week is unrestricted — a restricted DOW can still match
        // via the OR-rule, so the schedule is not actually impossible.
        if cron.dow.wildcard && !cron.dom.wildcard {
            let max_day = |m: u32| match m {
                2 => 29,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            let reachable = cron
                .month
                .allowed
                .iter()
                .any(|&m| cron.dom.allowed.iter().any(|&d| d <= max_day(m)));
            if !reachable {
                return Err(Error::Schedule(format!(
                    "day-of-month never occurs in the selected month(s): `{expr}`"
                )));
            }
        }
        Ok(cron)
    }

    /// Next fire strictly after `after`, searching minute-by-minute up to ~4 years.
    pub fn next_after(&self, after: OffsetDateTime) -> Option<OffsetDateTime> {
        let after = after.to_offset(time::UtcOffset::UTC);
        // Truncate to the minute, then step to the first minute strictly after `after`.
        let base = after
            .date()
            .with_hms(after.hour(), after.minute(), 0)
            .ok()?
            .assume_utc();
        let mut candidate = base.checked_add(time::Duration::minutes(1))?;

        // Upper bound: 8 years of minutes — covers the widest Feb-29 gap, which is
        // 8 years across a non-leap century year (e.g. 2096 -> 2104).
        for _ in 0..(366 * 24 * 60 * 8) {
            if self.matches(candidate) {
                return Some(candidate);
            }
            candidate = candidate.checked_add(time::Duration::minutes(1))?;
        }
        None
    }

    fn matches(&self, dt: OffsetDateTime) -> bool {
        if !self.minute.matches(dt.minute() as u32) {
            return false;
        }
        if !self.hour.matches(dt.hour() as u32) {
            return false;
        }
        if !self.month.matches(u8::from(dt.month()) as u32) {
            return false;
        }

        let dom_ok = self.dom.matches(dt.day() as u32);
        let dow_ok = self
            .dow
            .matches(dt.weekday().number_days_from_sunday() as u32);

        // Vixie cron rule: if either day field begins with `*` the two are ANDed;
        // if both are restricted they are ORed. (For a bare `*`, dom_ok/dow_ok is
        // trivially true, so AND collapses to the other field.)
        if self.dom.wildcard || self.dow.wildcard {
            dom_ok && dow_ok
        } else {
            dom_ok || dow_ok
        }
    }
}

const MONTHS: &[(&str, u32)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const DAYS: &[(&str, u32)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

fn lookup_name(name: &str, table: &[(&str, u32)]) -> Option<u32> {
    let name = name.to_ascii_lowercase();
    table.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

fn parse_dow(spec: &str) -> Result<Field> {
    // day-of-week: 0-7 where 0 and 7 are both Sunday; names sun..sat.
    let mut field = parse_field(spec, 0, 7, DAYS)?;
    let mut normalized: Vec<u32> = field
        .allowed
        .iter()
        .map(|&v| if v == 7 { 0 } else { v })
        .collect();
    normalized.sort_unstable();
    normalized.dedup();
    field.allowed = normalized;
    Ok(field)
}

fn parse_field(spec: &str, min: u32, max: u32, names: &[(&str, u32)]) -> Result<Field> {
    let mut wildcard = false;
    let mut set: Vec<u32> = Vec::new();

    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(Error::Schedule(format!("empty term in field `{spec}`")));
        }

        // Split optional `/step`.
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| Error::Schedule(format!("invalid step `{s}` in `{spec}`")))?;
                if step == 0 {
                    return Err(Error::Schedule(format!("step must be > 0 in `{spec}`")));
                }
                (r, Some(step))
            }
            None => (part, None),
        };

        let (lo, hi, is_star) = if range_part == "*" {
            (min, max, true)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a = parse_value(a, names, min, max, spec)?;
            let b = parse_value(b, names, min, max, spec)?;
            if a > b {
                return Err(Error::Schedule(format!(
                    "range start {a} > end {b} in `{spec}`"
                )));
            }
            (a, b, false)
        } else {
            let v = parse_value(range_part, names, min, max, spec)?;
            match step {
                // `N/step` means from N to max stepping by step.
                Some(_) => (v, max, false),
                None => (v, v, false),
            }
        };

        // POSIX/Vixie: a field containing `*` is "unrestricted" for the
        // day-of-month vs day-of-week rule, even with a step (e.g. `*/2`).
        if is_star {
            wildcard = true;
        }

        let step = step.unwrap_or(1);
        let mut v = lo;
        while v <= hi {
            set.push(v);
            match v.checked_add(step) {
                Some(n) => v = n,
                None => break,
            }
        }
    }

    if set.is_empty() {
        return Err(Error::Schedule(format!("no values in field `{spec}`")));
    }
    set.sort_unstable();
    set.dedup();
    Ok(Field {
        allowed: set,
        wildcard,
    })
}

fn parse_value(s: &str, names: &[(&str, u32)], min: u32, max: u32, spec: &str) -> Result<u32> {
    let v = if let Ok(n) = s.parse::<u32>() {
        n
    } else if let Some(n) = lookup_name(s, names) {
        n
    } else {
        return Err(Error::Schedule(format!("invalid value `{s}` in `{spec}`")));
    };
    if v < min || v > max {
        return Err(Error::Schedule(format!(
            "value {v} out of range {min}-{max} in `{spec}`"
        )));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn next(expr: &str, from: OffsetDateTime) -> OffsetDateTime {
        Schedule::parse(expr)
            .unwrap()
            .next_after(from)
            .expect("a next fire")
    }

    #[test]
    fn parse_every_3_minutes() {
        let s = Schedule::parse("*/3 * * * *").unwrap();
        let from = datetime!(2026-06-25 12:00:30 UTC);
        assert_eq!(
            s.next_after(from).unwrap(),
            datetime!(2026-06-25 12:03:00 UTC)
        );
    }

    #[test]
    fn staggered_offset_minute_one() {
        // m2-style offset: minutes 1,4,7,...
        let from = datetime!(2026-06-25 12:00:00 UTC);
        assert_eq!(
            next("1-59/3 * * * *", from),
            datetime!(2026-06-25 12:01:00 UTC)
        );
    }

    #[test]
    fn step_from_zero_lands_on_next_slot() {
        let from = datetime!(2026-06-25 12:01:10 UTC);
        assert_eq!(
            next("*/3 * * * *", from),
            datetime!(2026-06-25 12:03:00 UTC)
        );
    }

    #[test]
    fn hour_rollover() {
        let from = datetime!(2026-06-25 12:59:10 UTC);
        assert_eq!(
            next("*/3 * * * *", from),
            datetime!(2026-06-25 13:00:00 UTC)
        );
    }

    #[test]
    fn daily_at_midnight() {
        let from = datetime!(2026-06-25 12:00:00 UTC);
        assert_eq!(next("@daily", from), datetime!(2026-06-26 00:00:00 UTC));
    }

    #[test]
    fn specific_time() {
        let from = datetime!(2026-06-25 09:15:00 UTC);
        assert_eq!(
            next("30 14 * * *", from),
            datetime!(2026-06-25 14:30:00 UTC)
        );
    }

    #[test]
    fn list_and_names_in_month() {
        let from = datetime!(2026-01-15 00:00:00 UTC);
        // First of March or June at 00:00.
        assert_eq!(
            next("0 0 1 mar,jun *", from),
            datetime!(2026-03-01 00:00:00 UTC)
        );
    }

    #[test]
    fn dow_or_dom_semantics() {
        // Both restricted -> OR. Day 13 OR Friday. 2026-02-13 is a Friday so both
        // hit; pick a month where the 13th is not Friday to prove OR.
        let from = datetime!(2026-03-01 00:00:00 UTC);
        // 2026-03-06 is the first Friday; 2026-03-13 is the 13th. OR -> Friday Mar 6.
        let got = next("0 0 13 * fri", from);
        assert_eq!(got, datetime!(2026-03-06 00:00:00 UTC));
    }

    #[test]
    fn dow_only_restricted_is_and_on_that_day() {
        let from = datetime!(2026-06-25 12:00:00 UTC); // Thursday
                                                       // Next Sunday 00:00.
        assert_eq!(
            next("0 0 * * sun", from),
            datetime!(2026-06-28 00:00:00 UTC)
        );
    }

    #[test]
    fn sunday_seven_equals_zero() {
        let a = Schedule::parse("0 0 * * 0").unwrap();
        let b = Schedule::parse("0 0 * * 7").unwrap();
        let from = datetime!(2026-06-25 12:00:00 UTC);
        assert_eq!(a.next_after(from), b.next_after(from));
    }

    #[test]
    fn interval_every_90s() {
        let s = Schedule::parse("@every 90s").unwrap();
        assert_eq!(s, Schedule::Interval(Duration::from_secs(90)));
        let from = datetime!(2026-06-25 12:00:00 UTC);
        assert_eq!(
            s.next_after(from).unwrap(),
            datetime!(2026-06-25 12:01:30 UTC)
        );
    }

    #[test]
    fn interval_compound() {
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800));
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(Schedule::parse("").is_err());
        assert!(Schedule::parse("* * * *").is_err());
        assert!(Schedule::parse("60 * * * *").is_err());
        assert!(Schedule::parse("*/0 * * * *").is_err());
        assert!(Schedule::parse("5-1 * * * *").is_err());
        assert!(Schedule::parse("@every 0s").is_err());
        assert!(Schedule::parse("@every banana").is_err());
        assert!(Schedule::parse("xyz * * * *").is_err());
    }

    #[test]
    fn leap_day_schedule_resolves() {
        // Feb 29 only exists on leap years; next from 2026 is 2028.
        let from = datetime!(2026-06-25 00:00:00 UTC);
        assert_eq!(next("0 0 29 2 *", from), datetime!(2028-02-29 00:00:00 UTC));
    }

    #[test]
    fn impossible_dates_are_rejected() {
        // Feb 30 / Apr 31 / Jun 31 can never occur with an unrestricted weekday.
        assert!(Schedule::parse("0 0 30 2 *").is_err());
        assert!(Schedule::parse("0 0 31 4 *").is_err());
        assert!(Schedule::parse("0 0 31 6 *").is_err());
        // ...but with a restricted weekday the OR-rule makes them valid.
        assert!(Schedule::parse("0 0 31 4 fri").is_ok());
        // and a real date is fine.
        assert!(Schedule::parse("0 0 29 2 *").is_ok());
        assert!(Schedule::parse("0 0 31 1 *").is_ok());
    }

    #[test]
    fn stepped_star_dom_ands_with_dow() {
        // `*/2` in day-of-month is a wildcard for the OR/AND rule, so with a
        // restricted weekday it ANDs: odd day-of-month AND Monday.
        let from = datetime!(2026-06-01 00:00:00 UTC); // Mon Jun 1 is day 1 (odd)
        assert_eq!(
            next("0 0 */2 * 1", from),
            datetime!(2026-06-15 00:00:00 UTC)
        );
    }

    #[test]
    fn absurd_interval_returns_none_not_panic() {
        // ~31,700 years overflows the supported date range — must not panic.
        let s = Schedule::parse("@every 999999999999s").unwrap();
        assert_eq!(s.next_after(datetime!(2026-06-25 12:00:00 UTC)), None);
    }

    #[test]
    fn pathological_cron_step_does_not_panic() {
        // Step within ~59 of u32::MAX must not overflow-panic during parse.
        let s = Schedule::parse("5/4294967295 * * * *").unwrap();
        // Only minute 5 is produced; next fire is the next :05.
        assert_eq!(
            s.next_after(datetime!(2026-06-25 12:00:00 UTC)),
            Some(datetime!(2026-06-25 12:05:00 UTC))
        );
    }
}
