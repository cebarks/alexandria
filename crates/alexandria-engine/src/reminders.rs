//! Pure reminder scheduling: spec parsing/validation, next-fire computation,
//! occurrence counting. No DB, no async — engine crate rules.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, NaiveTime, Utc, Weekday};
use chrono_tz::Tz;
use cron::Schedule;
use std::str::FromStr;

/// Hard cap on occurrence iteration; guards against pathological expressions.
const MAX_ITER: usize = 10_000;

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

/// Full weekday name for human-facing output ("Friday"). Distinct from
/// `weekday_token`'s cron form ("FRI"): `chrono::Weekday`'s Debug and Display
/// impls both render only the three-letter form.
fn weekday_full_name(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
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

/// Compile any recurring spec to a `cron::Schedule`.
fn compiled_schedule(spec: &ScheduleSpec) -> Result<Schedule> {
    let expr = match spec {
        ScheduleSpec::Once { .. } => bail!("Once has no recurring schedule"),
        ScheduleSpec::Pattern {
            freq,
            time,
            weekdays,
            day_of_month,
        } => pattern_to_cron(
            *freq,
            *time,
            weekdays,
            (*freq == Freq::Monthly).then_some(*day_of_month),
        )?,
        ScheduleSpec::Cron { expr } => expr.clone(),
    };
    Ok(Schedule::from_str(&expr)?)
}

/// First fire strictly after `after`, in UTC. `None` for past one-shots or
/// expressions that never fire again. Recurring evaluation happens in `tz`
/// (wall-clock semantics); the cron crate + chrono-tz handle DST. `cron`
/// iterates local date/time fields and skips local times that don't exist, so
/// a daily 09:00 stays 09:00 local across a transition (verified by
/// `daily_series_across_dst_spring_forward`) — no manual chrono-tz fallback
/// needed. On a fall-back day a local time inside the repeated hour is yielded
/// twice by `cron` (both offsets); coalescing in `occurrences_between` absorbs
/// the duplicate rather than dropping a fire.
pub fn next_fire(
    spec: &ScheduleSpec,
    after: DateTime<Utc>,
    tz: Tz,
) -> Result<Option<DateTime<Utc>>> {
    match spec {
        ScheduleSpec::Once { due_at } => Ok((*due_at > after).then_some(*due_at)),
        _ => {
            let schedule = compiled_schedule(spec)?;
            let after_local = after.with_timezone(&tz);
            Ok(schedule
                .after(&after_local)
                .next()
                .map(|dt| dt.with_timezone(&Utc)))
        }
    }
}

/// Next `n` fire times strictly after `from` (preview for set_reminder).
/// `Once` yields `[due_at]` if still in the future, else empty.
pub fn upcoming(
    spec: &ScheduleSpec,
    from: DateTime<Utc>,
    tz: Tz,
    n: usize,
) -> Result<Vec<DateTime<Utc>>> {
    match spec {
        ScheduleSpec::Once { due_at } => Ok(if *due_at > from {
            vec![*due_at]
        } else {
            Vec::new()
        }),
        _ => {
            let schedule = compiled_schedule(spec)?;
            let from_local = from.with_timezone(&tz);
            Ok(schedule
                .after(&from_local)
                .take(n)
                .map(|dt| dt.with_timezone(&Utc))
                .collect())
        }
    }
}

/// Count occurrences strictly after `after_exclusive` and <= `until_inclusive`.
/// Used for coalescing: how many fires were missed while nobody consumed.
/// Always 0 for `Once` (its single occurrence *is* the delivery).
/// Saturates at MAX_ITER to bound pathological expressions.
pub fn occurrences_between(
    spec: &ScheduleSpec,
    after_exclusive: DateTime<Utc>,
    until_inclusive: DateTime<Utc>,
    tz: Tz,
) -> Result<u64> {
    if matches!(spec, ScheduleSpec::Once { .. }) || until_inclusive <= after_exclusive {
        return Ok(0);
    }
    let schedule = compiled_schedule(spec)?;
    let from_local = after_exclusive.with_timezone(&tz);
    let mut count = 0u64;
    for dt in schedule.after(&from_local).take(MAX_ITER) {
        if dt.with_timezone(&Utc) > until_inclusive {
            break;
        }
        count += 1;
    }
    Ok(count)
}

/// Short human rendering for list/set responses.
pub fn human_readable(spec: &ScheduleSpec) -> String {
    match spec {
        ScheduleSpec::Once { due_at } => {
            format!("one-shot at {}", due_at.format("%Y-%m-%d %H:%M UTC"))
        }
        ScheduleSpec::Pattern {
            freq,
            time,
            weekdays,
            day_of_month,
        } => {
            let hm = time.format("%H:%M");
            match freq {
                Freq::Daily => format!("daily at {hm}"),
                Freq::Weekly => {
                    // Full names ("Friday"), not `{w:?}` (which renders "Fri"):
                    // this string is user-facing and is asserted verbatim by the
                    // MCP layer and the pi companion extension.
                    let days: Vec<&str> = weekdays.iter().map(|w| weekday_full_name(*w)).collect();
                    format!("every {} at {hm}", days.join(", "))
                }
                Freq::Monthly => format!("monthly on day {day_of_month} at {hm}"),
            }
        }
        ScheduleSpec::Cron { expr } => format!("cron '{expr}'"),
    }
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
    fn parse_freq_accepts_names_case_insensitive() {
        assert_eq!(parse_freq("daily").unwrap(), Freq::Daily);
        assert_eq!(parse_freq("WEEKLY").unwrap(), Freq::Weekly);
        assert_eq!(parse_freq("Monthly").unwrap(), Freq::Monthly);
        assert!(parse_freq("hourly").is_err());
        assert!(parse_freq("").is_err());
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

    #[test]
    fn parse_datetime_rejects_ambiguous_dst_fold_time() {
        // America/New_York falls back 2026-11-01: 01:30 local occurs twice.
        // Strict `.single()` must reject it — a regression to `.earliest()`
        // would silently pick one of the two instants.
        assert!(parse_datetime("2026-11-01T01:30", nyc()).is_err());
    }

    fn nyc() -> Tz {
        "America/New_York".parse().unwrap()
    }
    // `y` is i32 because `TimeZone::with_ymd_and_hms` takes the year as i32.
    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn next_fire_once() {
        let spec = ScheduleSpec::Once {
            due_at: utc(2026, 9, 10, 15, 0),
        };
        assert_eq!(
            next_fire(&spec, utc(2026, 9, 10, 14, 0), Tz::UTC).unwrap(),
            Some(utc(2026, 9, 10, 15, 0))
        );
        // already past → None
        assert_eq!(
            next_fire(&spec, utc(2026, 9, 10, 16, 0), Tz::UTC).unwrap(),
            None
        );
    }

    #[test]
    fn next_fire_daily_pattern_in_tz() {
        // daily 09:00 New York; ask at 2026-09-10 14:00 UTC (10:00 EDT — past today's fire)
        let spec = ScheduleSpec::Pattern {
            freq: Freq::Daily,
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            weekdays: vec![],
            day_of_month: 1,
        };
        let next = next_fire(&spec, utc(2026, 9, 10, 14, 0), nyc())
            .unwrap()
            .unwrap();
        // 2026-09-11 09:00 EDT = 13:00 UTC
        assert_eq!(next, utc(2026, 9, 11, 13, 0));
    }

    #[test]
    fn next_fire_monthly_skips_short_months() {
        // monthly on the 31st at 10:00 UTC; from Sep 1 → Oct 31 (Sep has 30 days)
        let spec = ScheduleSpec::Pattern {
            freq: Freq::Monthly,
            time: NaiveTime::from_hms_opt(10, 0, 0).unwrap(),
            weekdays: vec![],
            day_of_month: 31,
        };
        let next = next_fire(&spec, utc(2026, 9, 1, 0, 0), Tz::UTC)
            .unwrap()
            .unwrap();
        assert_eq!(next, utc(2026, 10, 31, 10, 0));
    }

    #[test]
    fn upcoming_returns_n_fire_times() {
        let spec = ScheduleSpec::Pattern {
            freq: Freq::Weekly,
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            weekdays: vec![Weekday::Fri],
            day_of_month: 1,
        };
        let ups = upcoming(&spec, utc(2026, 9, 1, 0, 0), Tz::UTC, 3).unwrap();
        assert_eq!(ups.len(), 3);
        assert!(ups[0] < ups[1] && ups[1] < ups[2]);
        use chrono::Datelike;
        assert!(ups.iter().all(|d| d.weekday() == Weekday::Fri));
    }

    #[test]
    fn occurrences_between_coalesces_downtime() {
        // daily 12:00 UTC; missed window Sep 1 12:00 (exclusive) → Sep 4 13:00
        let spec = ScheduleSpec::Pattern {
            freq: Freq::Daily,
            time: NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
            weekdays: vec![],
            day_of_month: 1,
        };
        // occurrences strictly after Sep 1 12:00, up to Sep 4 13:00 = Sep 2, 3, 4 → 3
        assert_eq!(
            occurrences_between(
                &spec,
                utc(2026, 9, 1, 12, 0),
                utc(2026, 9, 4, 13, 0),
                Tz::UTC
            )
            .unwrap(),
            3
        );
        // none in an empty window
        assert_eq!(
            occurrences_between(
                &spec,
                utc(2026, 9, 1, 12, 0),
                utc(2026, 9, 1, 13, 0),
                Tz::UTC
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn occurrences_between_once_is_zero() {
        let spec = ScheduleSpec::Once {
            due_at: utc(2026, 9, 10, 15, 0),
        };
        assert_eq!(
            occurrences_between(
                &spec,
                utc(2026, 9, 1, 0, 0),
                utc(2026, 9, 20, 0, 0),
                Tz::UTC
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn daily_series_across_dst_spring_forward() {
        // America/New_York springs forward 2026-03-08 02:00 → 03:00.
        // daily 09:00 local must fire exactly once per day, strictly increasing,
        // always at 09:00 local wall time (UTC offset shifts: EST is UTC-5, EDT UTC-4).
        let spec = ScheduleSpec::Pattern {
            freq: Freq::Daily,
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            weekdays: vec![],
            day_of_month: 1,
        };
        let ups = upcoming(&spec, utc(2026, 3, 6, 0, 0), nyc(), 4).unwrap();
        assert_eq!(ups.len(), 4);
        assert!(ups.windows(2).all(|w| w[0] < w[1]));
        // Mar 6, 7 = EST (UTC-5) → 14:00 UTC; Mar 8, 9 = EDT (UTC-4) → 13:00 UTC
        assert_eq!(ups[0], utc(2026, 3, 6, 14, 0));
        assert_eq!(ups[1], utc(2026, 3, 7, 14, 0));
        assert_eq!(ups[2], utc(2026, 3, 8, 13, 0));
        assert_eq!(ups[3], utc(2026, 3, 9, 13, 0));
    }

    #[test]
    fn human_readable_renders_all_kinds() {
        assert_eq!(
            human_readable(&ScheduleSpec::Once {
                due_at: utc(2026, 9, 10, 15, 0)
            }),
            "one-shot at 2026-09-10 15:00 UTC"
        );
        assert_eq!(
            human_readable(&ScheduleSpec::Pattern {
                freq: Freq::Weekly,
                time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                weekdays: vec![Weekday::Fri],
                day_of_month: 1,
            }),
            "every Friday at 09:00"
        );
        assert_eq!(
            human_readable(&ScheduleSpec::Cron {
                expr: "0 0 9 * * 1-5".to_string()
            }),
            "cron '0 0 9 * * 1-5'"
        );
    }
}
