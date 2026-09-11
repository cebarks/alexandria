//! Pure reminder scheduling: spec parsing/validation, next-fire computation,
//! occurrence counting. No DB, no async — engine crate rules.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, NaiveTime, Utc, Weekday};
use chrono_tz::Tz;
use cron::Schedule;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
}

/// A validated reminder schedule.
#[derive(Debug, Clone, PartialEq)]
pub enum ScheduleSpec {
    Once {
        due_at: DateTime<Utc>,
    },
    Pattern {
        freq: Freq,
        time: NaiveTime,
        /// Non-empty iff freq == Weekly.
        weekdays: Vec<Weekday>,
        /// 1..=31, used iff freq == Monthly. Short months are skipped (cron semantics).
        day_of_month: u32,
    },
    Cron {
        /// Normalized 6- or 7-field expression (seconds first).
        expr: String,
    },
}

pub fn parse_freq(s: &str) -> Result<Freq> {
    match s.to_ascii_lowercase().as_str() {
        "daily" => Ok(Freq::Daily),
        "weekly" => Ok(Freq::Weekly),
        "monthly" => Ok(Freq::Monthly),
        other => bail!("freq must be daily|weekly|monthly, got {other:?}"),
    }
}

pub fn parse_weekday(s: &str) -> Result<Weekday> {
    // Three-letter prefixes and full names, case-insensitive. Numbers are
    // deliberately rejected: 0-vs-1 Sunday conventions differ across cron
    // dialects and ambiguity here means silent misfires.
    let m = match s.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        other => bail!("invalid weekday {other:?}; use mon..sun"),
    };
    Ok(m)
}

/// Parse `HH:MM` (24-hour). Seconds are always zero — reminder wall-clock
/// granularity is the minute.
pub fn parse_time_of_day(s: &str) -> Result<NaiveTime> {
    let (h, m) = s
        .split_once(':')
        .with_context(|| format!("time must be HH:MM, got {s:?}"))?;
    let (h, m): (u32, u32) = (
        h.parse().context("invalid hour")?,
        m.parse().context("invalid minute")?,
    );
    if h > 23 || m > 59 {
        bail!("time out of range: {s:?}");
    }
    NaiveTime::from_hms_opt(h, m, 0).with_context(|| format!("invalid time {s:?}"))
}

/// Validate a user-supplied cron string (5, 6, or 7 fields) and normalize it to
/// the cron crate's seconds-first form. 5-field input gets `0 ` prepended.
pub fn normalize_cron(expr: &str) -> Result<String> {
    let fields = expr.split_whitespace().count();
    let normalized = match fields {
        5 => format!("0 {expr}"),
        6 | 7 => expr.trim().to_string(),
        n => bail!("cron expression must have 5-7 fields, got {n}"),
    };
    Schedule::from_str(&normalized).with_context(|| format!("invalid cron expression {expr:?}"))?;
    Ok(normalized)
}

fn weekday_token(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "MON",
        Weekday::Tue => "TUE",
        Weekday::Wed => "WED",
        Weekday::Thu => "THU",
        Weekday::Fri => "FRI",
        Weekday::Sat => "SAT",
        Weekday::Sun => "SUN",
    }
}

/// Compile a named pattern to a normalized (seconds-first) cron expression.
/// Validates field combinations: Weekly needs ≥1 weekday, Monthly needs
/// day_of_month 1..=31, Daily takes neither.
pub fn pattern_to_cron(
    freq: Freq,
    time: NaiveTime,
    weekdays: &[Weekday],
    day_of_month: Option<u32>,
) -> Result<String> {
    use chrono::Timelike;
    let (h, m) = (time.hour(), time.minute());
    let expr = match freq {
        Freq::Daily => {
            if !weekdays.is_empty() || day_of_month.is_some() {
                bail!("daily pattern takes no weekdays/day_of_month");
            }
            format!("0 {m} {h} * * *")
        }
        Freq::Weekly => {
            if weekdays.is_empty() {
                bail!("weekly pattern requires at least one weekday");
            }
            if day_of_month.is_some() {
                bail!("weekly pattern takes no day_of_month");
            }
            let dows: Vec<&str> = weekdays.iter().map(|w| weekday_token(*w)).collect();
            format!("0 {m} {h} * * {}", dows.join(","))
        }
        Freq::Monthly => {
            let d = match day_of_month {
                Some(d @ 1..=31) => d,
                Some(other) => bail!("day_of_month must be 1-31, got {other}"),
                None => bail!("monthly pattern requires day_of_month"),
            };
            if !weekdays.is_empty() {
                bail!("monthly pattern takes no weekdays");
            }
            format!("0 {m} {h} {d} * *")
        }
    };
    Schedule::from_str(&expr).context("internal error: compiled pattern is invalid cron")?;
    Ok(expr)
}

/// Parse an ISO-8601-ish datetime. Explicit offsets are honored; naive
/// timestamps are interpreted in `tz` (documented server behavior).
/// Errors on nonexistent local times (DST gaps) and on ambiguous ones (folds)
/// rather than guessing — callers get a clear message and can pick an
/// unambiguous time.
pub fn parse_datetime(input: &str, tz: Tz) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.with_timezone(&Utc));
    }
    const FORMATS: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ];
    let naive = FORMATS
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(input, f).ok())
        .with_context(|| {
            format!("could not parse datetime {input:?}; use ISO-8601, e.g. 2026-09-10T15:00:00Z")
        })?;
    use chrono::TimeZone;
    let local = tz
        .from_local_datetime(&naive)
        .single()
        .with_context(|| format!("local time {input:?} is ambiguous or does not exist in {tz}"))?;
    Ok(local.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn normalize_cron_prepends_seconds_to_five_fields() {
        assert_eq!(normalize_cron("0 9 * * 1-5").unwrap(), "0 0 9 * * 1-5");
        assert_eq!(normalize_cron("0 0 9 * * 1-5").unwrap(), "0 0 9 * * 1-5");
    }

    #[test]
    fn normalize_cron_rejects_garbage() {
        assert!(normalize_cron("not a cron").is_err());
        assert!(normalize_cron("0 9 * *").is_err()); // 4 fields
        assert!(normalize_cron("99 99 99 99 99").is_err());
    }

    #[test]
    fn pattern_to_cron_all_freqs() {
        let t = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        assert_eq!(
            pattern_to_cron(Freq::Daily, t, &[], None).unwrap(),
            "0 0 9 * * *"
        );
        assert_eq!(
            pattern_to_cron(Freq::Weekly, t, &[Weekday::Fri], None).unwrap(),
            "0 0 9 * * FRI"
        );
        assert_eq!(
            pattern_to_cron(Freq::Weekly, t, &[Weekday::Mon, Weekday::Fri], None).unwrap(),
            "0 0 9 * * MON,FRI"
        );
        assert_eq!(
            pattern_to_cron(Freq::Monthly, t, &[], Some(1)).unwrap(),
            "0 0 9 1 * *"
        );
        // validation failures
        assert!(pattern_to_cron(Freq::Weekly, t, &[], None).is_err()); // no weekdays
        assert!(pattern_to_cron(Freq::Monthly, t, &[], None).is_err()); // no day
        assert!(pattern_to_cron(Freq::Monthly, t, &[], Some(32)).is_err());
        assert!(pattern_to_cron(Freq::Daily, t, &[Weekday::Fri], None).is_err());
        // weekdays on daily
    }

    #[test]
    fn parse_weekday_accepts_names_case_insensitive() {
        assert_eq!(parse_weekday("fri").unwrap(), Weekday::Fri);
        assert_eq!(parse_weekday("MONDAY").unwrap(), Weekday::Mon);
        assert_eq!(parse_weekday("sun").unwrap(), Weekday::Sun);
        assert!(parse_weekday("funday").is_err());
        assert!(parse_weekday("7").is_err()); // numbers rejected — names only, unambiguous
    }

    #[test]
    fn parse_time_of_day_formats() {
        assert_eq!(
            parse_time_of_day("09:00").unwrap(),
            NaiveTime::from_hms_opt(9, 0, 0).unwrap()
        );
        assert_eq!(
            parse_time_of_day("9:30").unwrap(),
            NaiveTime::from_hms_opt(9, 30, 0).unwrap()
        );
        assert!(parse_time_of_day("25:00").is_err());
        assert!(parse_time_of_day("noon").is_err());
    }

    #[test]
    fn parse_datetime_offset_wins_over_tz() {
        let tz: Tz = "Europe/Stockholm".parse().unwrap();
        let dt = parse_datetime("2026-09-10T15:00:00+02:00", tz).unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2026, 9, 10, 13, 0, 0).unwrap());
    }

    #[test]
    fn parse_datetime_naive_uses_configured_tz() {
        let tz: Tz = "Europe/Stockholm".parse().unwrap();
        // September = CEST (+02:00)
        let dt = parse_datetime("2026-09-10T15:00", tz).unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2026, 9, 10, 13, 0, 0).unwrap());
        let dt2 = parse_datetime("2026-09-10 15:00:00", tz).unwrap();
        assert_eq!(dt, dt2);
    }

    #[test]
    fn parse_datetime_rejects_nonexistent_dst_gap_time() {
        // America/New_York springs forward 2026-03-08: 02:30 local does not exist
        let tz: Tz = "America/New_York".parse().unwrap();
        assert!(parse_datetime("2026-03-08T02:30", tz).is_err());
    }
}
