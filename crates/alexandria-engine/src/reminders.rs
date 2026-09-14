//! Pure reminder scheduling: spec parsing/validation, next-fire computation,
//! occurrence counting. No DB, no async — engine crate rules.

use alexandria_storage::models::schedule_kind;
use anyhow::{anyhow, bail, Context, Result};
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
    // Name the field and echo the input: callers surface only the top-level
    // context, so "invalid minute" alone leaves the user guessing which of
    // HH:MM was rejected (or what was passed).
    let (h, m): (u32, u32) = (
        h.parse()
            .with_context(|| format!("invalid hour in time {s:?}"))?,
        m.parse()
            .with_context(|| format!("invalid minute in time {s:?}"))?,
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
        n => bail!("cron expression must have 5-7 fields, got {n}: {expr:?}"),
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
/// timestamps are interpreted in `tz` (documented server behavior). Errors on
/// nonexistent local times (a spring-forward gap) and on ambiguous ones (a
/// fall-back fold) rather than guessing. The two need opposite fixes, so they
/// get opposite messages: a gap time has to be moved, a fold time only has to
/// be pinned to an offset.
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
    use chrono::{LocalResult, TimeZone};
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(local) => Ok(local.with_timezone(&Utc)),
        LocalResult::None => bail!(
            "local time {input:?} does not exist in {tz} (spring-forward gap); pick a time after the transition"
        ),
        LocalResult::Ambiguous(..) => bail!(
            "local time {input:?} is ambiguous in {tz} (fall-back fold); specify a UTC offset instead"
        ),
    }
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

/// Reconstruct a validated spec from the flat storage row. Errors on corrupt
/// or inconsistent rows (never panics). Every error names the row, so a caller
/// iterating a batch (e.g. the delivery loop over `list_due`) can identify — and
/// isolate — the single bad row instead of reporting an anonymous failure.
///
/// This is the row→spec direction; the spec→row mapping built by the reminder
/// writer must stay field-for-field consistent with it (including cron
/// normalization and the `day_of_month`/`weekdays` per-freq rules).
pub fn spec_from_reminder(r: &alexandria_storage::models::Reminder) -> Result<ScheduleSpec> {
    spec_from_row(r).map_err(|e| {
        let id =
            r.id.as_ref()
                .map(alexandria_storage::record_id_to_string)
                .unwrap_or_else(|| "<no id>".to_string());
        // `{e:#}` flattens the cause chain onto one line: callers surface errors
        // through `Display` (the MCP layer JSON-encodes `e.to_string()`), so the
        // row id must not hide which field actually failed.
        anyhow!("reminder {id}: {e:#}")
    })
}

/// Kind-specific field extraction. `schedule_kind` discriminators come from
/// [`schedule_kind`], the same constants the storage schema's `ASSERT` mirrors.
fn spec_from_row(r: &alexandria_storage::models::Reminder) -> Result<ScheduleSpec> {
    match r.schedule_kind.as_str() {
        schedule_kind::ONCE => Ok(ScheduleSpec::Once {
            due_at: r.due_at.context("once reminder missing due_at")?,
        }),
        schedule_kind::PATTERN => {
            let freq = parse_freq(r.freq.as_deref().context("pattern missing freq")?)?;
            let time = parse_time_of_day(
                r.time_of_day
                    .as_deref()
                    .context("pattern missing time_of_day")?,
            )?;
            let weekdays: Vec<Weekday> = r
                .weekdays
                .iter()
                .map(|w| parse_weekday(w).with_context(|| format!("stored weekdays entry {w:?}")))
                .collect::<Result<Vec<_>>>()?;
            // day_of_month is only meaningful for Monthly, but the flat row
            // carries the column for every freq: reject junk whenever present,
            // require a value for Monthly (guessing "the 1st" would silently
            // misfire), and default only where the field is genuinely unused.
            let day_of_month = match r.day_of_month {
                Some(d) if !(1..=31).contains(&d) => bail!("day_of_month out of range: {d}"),
                Some(d) => d as u32,
                None if freq == Freq::Monthly => bail!("monthly pattern missing day_of_month"),
                None => 1, // unused for Daily/Weekly; ScheduleSpec requires a concrete u32
            };
            // Re-run pattern validation so cross-field inconsistencies (weekly
            // with no weekdays, daily/monthly carrying weekdays) fail here
            // instead of later inside next_fire. day_of_month only applies to
            // Monthly; the flat row carries a default for the other freqs.
            pattern_to_cron(
                freq,
                time,
                &weekdays,
                (freq == Freq::Monthly).then_some(day_of_month),
            )?;
            Ok(ScheduleSpec::Pattern {
                freq,
                time,
                weekdays,
                day_of_month,
            })
        }
        schedule_kind::CRON => {
            let expr = r
                .cron_expr
                .as_deref()
                .context("cron reminder missing cron_expr")?;
            Ok(ScheduleSpec::Cron {
                expr: normalize_cron(expr)?,
            })
        }
        other => bail!("unknown schedule_kind {other:?}"),
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

    /// The `cron` crate numbers days of week 1=Sunday..7=Saturday and rejects
    /// `0` — unlike standard/Vixie cron's 0-7 with 0=Sunday. Nothing else here
    /// pins that, and the wrong reading is silent rather than an error: `1-5`
    /// fires Sunday–Thursday, never Friday. Both steering surfaces
    /// (`set_reminder`'s `cron` param description and the memory skill) tell
    /// callers to write day names because of this, so the numbering is load-
    /// bearing behavior, not a detail of a third-party crate.
    #[test]
    fn cron_numeric_weekdays_are_sunday_first_and_reject_zero() {
        use chrono::Datelike;
        let fires = |expr: &str| -> Vec<Weekday> {
            upcoming(
                &ScheduleSpec::Cron {
                    expr: expr.to_string(),
                },
                utc(2026, 9, 7, 0, 0), // a Monday
                Tz::UTC,
                7,
            )
            .unwrap()
            .iter()
            .map(|d| d.weekday())
            .collect()
        };

        assert_eq!(fires("0 0 9 * * 1"), vec![Weekday::Sun; 7]);
        assert_eq!(fires("0 0 9 * * 7"), vec![Weekday::Sat; 7]);
        assert!(normalize_cron("0 0 9 * * 0").is_err());

        // The trap a standard-cron reader falls into: Sunday–Thursday.
        assert_eq!(
            fires("0 0 9 * * 1-5"),
            vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Sun,
                Weekday::Mon,
                Weekday::Tue,
            ]
        );

        // Day names are unambiguous — the form `pattern_to_cron` emits.
        assert_eq!(
            fires("0 0 9 * * MON-FRI"),
            vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Mon,
                Weekday::Tue,
            ]
        );
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

    /// A spring-forward gap has no such instant: the message must say so and
    /// point at the fix (move the time), and must not read like the fold case.
    #[test]
    fn parse_datetime_rejects_nonexistent_dst_gap_time() {
        // America/New_York springs forward 2026-03-08: 02:30 local does not exist
        let tz: Tz = "America/New_York".parse().unwrap();
        let err = parse_datetime("2026-03-08T02:30", tz)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("spring-forward gap"), "{err}");
        assert!(err.contains("pick a time after the transition"), "{err}");
        // The fold wording belongs to the other failure mode only.
        assert!(!err.contains("ambiguous"), "{err}");
    }

    /// A fall-back fold names two instants, so moving the time would not help:
    /// the message must say it is ambiguous and point at the offset fix.
    #[test]
    fn parse_datetime_rejects_ambiguous_dst_fold_time() {
        // America/New_York falls back 2026-11-01: 01:30 local occurs twice.
        // Strict branch on `LocalResult` must reject it — a regression to
        // `.earliest()` would silently pick one of the two instants.
        let err = parse_datetime("2026-11-01T01:30", nyc())
            .unwrap_err()
            .to_string();
        assert!(err.contains("is ambiguous"), "{err}");
        assert!(err.contains("fall-back fold"), "{err}");
        assert!(err.contains("specify a UTC offset instead"), "{err}");
        assert!(!err.contains("does not exist"), "{err}");
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

    /// The set-time path reads the next fire out of the same `upcoming` walk it
    /// uses for the preview, so the two must agree on the first element for every
    /// spec kind and zone — otherwise which function a caller happened to use
    /// would decide the stored `next_due_at`.
    #[test]
    fn next_fire_is_the_first_element_of_upcoming() {
        let daily = || NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let specs = [
            ScheduleSpec::Once {
                due_at: utc(2026, 9, 10, 15, 0),
            },
            // A one-shot already in the past: no future fire in either form.
            ScheduleSpec::Once {
                due_at: utc(2020, 1, 1, 0, 0),
            },
            ScheduleSpec::Pattern {
                freq: Freq::Daily,
                time: daily(),
                weekdays: vec![],
                day_of_month: 1,
            },
            ScheduleSpec::Pattern {
                freq: Freq::Weekly,
                time: daily(),
                weekdays: vec![Weekday::Fri],
                day_of_month: 1,
            },
            // Monthly on the 31st: most months are skipped, so the walk matters.
            ScheduleSpec::Pattern {
                freq: Freq::Monthly,
                time: daily(),
                weekdays: vec![],
                day_of_month: 31,
            },
            // Pinned to a year already past: never fires again.
            ScheduleSpec::Cron {
                expr: "0 0 12 * * * 2020".to_string(),
            },
            // 01:30 local: the one window where a DST fold yields two instants.
            ScheduleSpec::Cron {
                expr: "0 30 1 * * *".to_string(),
            },
        ];
        let from = utc(2026, 3, 7, 12, 0);
        for spec in &specs {
            for zone in [Tz::UTC, nyc()] {
                assert_eq!(
                    next_fire(spec, from, zone).unwrap(),
                    upcoming(spec, from, zone, 3).unwrap().first().copied(),
                    "{spec:?} in {zone}"
                );
            }
        }
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

    #[test]
    fn spec_from_reminder_roundtrips_all_kinds() {
        use alexandria_storage::models::Reminder;

        fn base() -> Reminder {
            Reminder {
                id: None,
                message: "m".into(),
                target_project: None,
                prov_project: None,
                prov_session_id: None,
                note: None,
                schedule_kind: "once".into(),
                due_at: None,
                freq: None,
                time_of_day: None,
                weekdays: vec![],
                day_of_month: None,
                cron_expr: None,
                next_due_at: None,
                status: "pending".into(),
                created_at: None,
                cancelled_at: None,
                last_delivered_at: None,
                delivered_count: 0,
            }
        }

        fn pattern() -> Reminder {
            let mut r = base();
            r.schedule_kind = schedule_kind::PATTERN.into();
            r.time_of_day = Some("09:00".into());
            r
        }

        let mut once = base();
        once.due_at = Some(utc(2026, 9, 10, 15, 0));
        assert_eq!(
            spec_from_reminder(&once).unwrap(),
            ScheduleSpec::Once {
                due_at: utc(2026, 9, 10, 15, 0)
            }
        );

        // Every field the conversion wires must come back exactly: day_of_month
        // is the flat row's default (unused for Weekly).
        let mut weekly = pattern();
        weekly.freq = Some("weekly".into());
        weekly.weekdays = vec!["fri".into()];
        assert_eq!(
            spec_from_reminder(&weekly).unwrap(),
            ScheduleSpec::Pattern {
                freq: Freq::Weekly,
                time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                weekdays: vec![Weekday::Fri],
                day_of_month: 1,
            }
        );

        let mut monthly = pattern();
        monthly.freq = Some("monthly".into());
        monthly.day_of_month = Some(15);
        assert_eq!(
            spec_from_reminder(&monthly).unwrap(),
            ScheduleSpec::Pattern {
                freq: Freq::Monthly,
                time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                weekdays: vec![],
                day_of_month: 15,
            }
        );

        let mut cr = base();
        cr.schedule_kind = schedule_kind::CRON.into();
        cr.cron_expr = Some("0 0 9 * * 1-5".into());
        assert_eq!(
            spec_from_reminder(&cr).unwrap(),
            ScheduleSpec::Cron {
                expr: "0 0 9 * * 1-5".into()
            }
        );

        // A stored 5-field expression must come back normalized to 6 fields —
        // `compiled_schedule` feeds `expr` straight to `Schedule::from_str`.
        let mut five = base();
        five.schedule_kind = schedule_kind::CRON.into();
        five.cron_expr = Some("0 9 * * 1-5".into());
        assert_eq!(
            spec_from_reminder(&five).unwrap(),
            ScheduleSpec::Cron {
                expr: "0 0 9 * * 1-5".into()
            }
        );

        // Corrupt row → error, not panic
        let mut bad = base();
        bad.schedule_kind = schedule_kind::PATTERN.into();
        assert!(spec_from_reminder(&bad).is_err());

        // Inconsistent row (weekly without weekdays) → error at conversion time,
        // not later inside next_fire.
        let mut no_days = pattern();
        no_days.freq = Some("weekly".into());
        assert!(spec_from_reminder(&no_days).is_err());

        // Monthly with no day_of_month: error rather than defaulting to the 1st,
        // which would silently misfire. Errors also name the row (id is None for
        // an unpersisted row) while keeping the specific reason on the same line,
        // so one corrupt row in a delivery batch is diagnosable via `to_string`.
        let mut no_dom = pattern();
        no_dom.freq = Some("monthly".into());
        let err = spec_from_reminder(&no_dom).unwrap_err().to_string();
        assert!(
            err.contains("monthly pattern missing day_of_month"),
            "{err}"
        );
        assert!(err.contains("reminder <no id>"), "{err}");

        // i64 -> u32 bounds: out-of-range must error, not truncate/wrap — both
        // for Monthly (where the value is used) and for a row carrying junk in
        // the otherwise-unused column, which pattern_to_cron never re-checks.
        for bad_dom in [0i64, -1, 32, i64::MAX] {
            for (freq, weekdays) in [("monthly", vec![]), ("weekly", vec!["fri".to_string()])] {
                let mut r = pattern();
                r.freq = Some(freq.into());
                r.weekdays = weekdays;
                r.day_of_month = Some(bad_dom);
                assert!(spec_from_reminder(&r).is_err(), "{freq} dom {bad_dom}");
            }
        }

        // A corrupt stored weekday reads as stored-row corruption, not user input.
        let mut bad_day = pattern();
        bad_day.freq = Some("weekly".into());
        bad_day.weekdays = vec!["xyz".into()];
        let err = spec_from_reminder(&bad_day).unwrap_err().to_string();
        assert!(err.contains("stored weekdays entry \"xyz\""), "{err}");
        assert!(err.contains("invalid weekday"), "{err}");
    }
}
