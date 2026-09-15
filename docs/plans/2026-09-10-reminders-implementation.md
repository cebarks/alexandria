# Alexandria Reminders Implementation Plan

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Agents can set one-shot and recurring reminders that are delivered (agent context + human notification) on the next interaction after they come due, with project targeting and an escalation backstop.

**Architecture:** Pure scheduling logic in `alexandria-engine` (new `reminders` module, no DB/async). Flat `reminder` table + `ReminderRepo` in `alexandria-storage` (migration v006). Four new MCP tools + a read-only `due_reminders` piggyback on retrieve/recall in `alexandria-mcp`. Delivery is 100% query-time — **no background timer anywhere**. The pi companion extension is renamed/generalized and gains a reminders feature that calls `check_reminders` per prompt.

**Tech Stack:** Rust (cron 0.17, chrono-tz 0.10, iana-time-zone 0.1), SurrealDB 3.2, rmcp 3.1, TypeScript pi extension.

**Design doc:** `docs/plans/2026-09-10-reminders-design.md` (in this worktree).

**Worktree:** `~/code/.worktrees/alexandria/feature/reminders` (branch `feature/reminders`).

**Conventions (from AGENTS.md/CLAUDE.md — read before starting):**

- SurrealDB 3.2 gotchas: never `SELECT value`; `DELETE reminder WHERE ...` (no FROM); bind pre-parsed `RecordId`s (never inline `type::record()` in RELATE); result structs need `#[derive(SurrealValue)]`; format ids with `record_id_to_string()`.
- Run `just check` / `just test` / `just lint` (clippy with `-Dwarnings`) before each commit; `just ci` at the end.
- Tool descriptions in `server.rs` are directive-style ("call this when...") — keep new ones consistent.
- Pre-commit hook note: `just install-hooks` copies into `.git/hooks`, but in a worktree `.git` is a file. If you want the hook here: `cp .githooks/pre-commit "$(git rev-parse --git-common-dir)/hooks/pre-commit"` (shared with main repo).

---

### Task 1: Workspace dependencies

**Files:**

- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/alexandria-engine/Cargo.toml`
- Modify: `crates/alexandria-mcp/Cargo.toml`
- Modify: `crates/alexandria/Cargo.toml`

**Step 1: Add workspace deps.** In root `Cargo.toml` `[workspace.dependencies]`, after `chrono = ...`:

```toml
chrono-tz = "0.10"
cron = "0.17"
iana-time-zone = "0.1"
```

**Step 2: Wire into crates.**

- `crates/alexandria-engine/Cargo.toml` `[dependencies]`: add `chrono-tz = { workspace = true }` and `cron = { workspace = true }`
- `crates/alexandria-mcp/Cargo.toml` `[dependencies]`: add `chrono-tz = { workspace = true }`
- `crates/alexandria/Cargo.toml` `[dependencies]`: add `chrono-tz = { workspace = true }` and `iana-time-zone = { workspace = true }`

**Step 3: Verify**

Run: `just check`
Expected: compiles clean (first run downloads/builds the new deps — a few minutes).

**Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/*/Cargo.toml
git commit -m "chore(deps): add cron, chrono-tz, iana-time-zone for reminders"
```

---

### Task 2: Engine — schedule spec, validation, datetime parsing

Pure logic, no DB. TDD.

**Files:**

- Create: `crates/alexandria-engine/src/reminders.rs`
- Modify: `crates/alexandria-engine/src/lib.rs` (add `pub mod reminders;` keeping alphabetical order)

**Step 1: Write the failing tests** — create `reminders.rs` with only the test module below (plus `use` stubs as needed to compile-fail on missing items):

```rust
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
        assert!(pattern_to_cron(Freq::Daily, t, &[Weekday::Fri], None).is_err()); // weekdays on daily
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
        assert_eq!(parse_time_of_day("09:00").unwrap(), NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        assert_eq!(parse_time_of_day("9:30").unwrap(), NaiveTime::from_hms_opt(9, 30, 0).unwrap());
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
```

**Step 2: Run to verify failure**

Run: `cargo test -p alexandria-engine reminders`
Expected: compile errors (functions don't exist).

**Step 3: Implement.** Full module header + implementation:

```rust
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

pub fn parse_time_of_day(s: &str) -> Result<NaiveTime> {
    use chrono::Timelike;
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
        .map(|t| { debug_assert!(t.second() == 0); t })
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
    Schedule::from_str(&normalized)
        .with_context(|| format!("invalid cron expression {expr:?}"))?;
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
/// Errors on nonexistent local times (DST gaps) rather than guessing.
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
        .with_context(|| format!("local time {input:?} does not exist in {tz} (DST gap)"))?;
    Ok(local.with_timezone(&Utc))
}
```

**Step 4: Run tests**

Run: `cargo test -p alexandria-engine reminders`
Expected: all Task-2 tests PASS.

> **Note on the DST-gap test**: `from_local_datetime(..).single()` returns `None` for gap times → error. For ambiguous (fold) times, `.single()` is also `None`; if you decide to accept folds, use `.earliest()` instead — but then the gap test needs `.earliest().context(...)` semantics too. The plan's choice: **strict `.single()`, error on both** — agents get a clear message and can pick an unambiguous time.

**Step 5: Commit**

```bash
git add crates/alexandria-engine/src/reminders.rs crates/alexandria-engine/src/lib.rs
git commit -m "feat(engine): reminder schedule spec, validation, datetime parsing"
```

---

### Task 3: Engine — next-fire, upcoming preview, occurrence coalescing

**Files:**

- Modify: `crates/alexandria-engine/src/reminders.rs`

**Step 1: Write the failing tests** (append to the test module):

```rust
    fn nyc() -> Tz { "America/New_York".parse().unwrap() }
    fn utc(y: u32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn next_fire_once() {
        let spec = ScheduleSpec::Once { due_at: utc(2026, 9, 10, 15, 0) };
        assert_eq!(next_fire(&spec, utc(2026, 9, 10, 14, 0), Tz::UTC).unwrap(), Some(utc(2026, 9, 10, 15, 0)));
        // already past → None
        assert_eq!(next_fire(&spec, utc(2026, 9, 10, 16, 0), Tz::UTC).unwrap(), None);
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
        let next = next_fire(&spec, utc(2026, 9, 10, 14, 0), nyc()).unwrap().unwrap();
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
        let next = next_fire(&spec, utc(2026, 9, 1, 0, 0), Tz::UTC).unwrap().unwrap();
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
        assert_eq!(occurrences_between(&spec, utc(2026, 9, 1, 12, 0), utc(2026, 9, 4, 13, 0), Tz::UTC).unwrap(), 3);
        // none in an empty window
        assert_eq!(occurrences_between(&spec, utc(2026, 9, 1, 12, 0), utc(2026, 9, 1, 13, 0), Tz::UTC).unwrap(), 0);
    }

    #[test]
    fn occurrences_between_once_is_zero() {
        let spec = ScheduleSpec::Once { due_at: utc(2026, 9, 10, 15, 0) };
        assert_eq!(occurrences_between(&spec, utc(2026, 9, 1, 0, 0), utc(2026, 9, 20, 0, 0), Tz::UTC).unwrap(), 0);
    }

    #[test]
    fn daily_series_across_dst_spring_forward() {
        // America/New_York springs forward 2026-03-08 02:00 → 03:00.
        // daily 09:00 local must fire exactly once per day, strictly increasing,
        // always at 09:00 local wall time (UTC offset shifts 13:00 → 14:00 wait: EDT is UTC-4, EST UTC-5).
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
            human_readable(&ScheduleSpec::Once { due_at: utc(2026, 9, 10, 15, 0) }),
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
            human_readable(&ScheduleSpec::Cron { expr: "0 0 9 * * 1-5".to_string() }),
            "cron '0 0 9 * * 1-5'"
        );
    }
```

**Step 2: Run to verify failure**

Run: `cargo test -p alexandria-engine reminders`
Expected: compile errors (`next_fire`, `upcoming`, `occurrences_between`, `human_readable` missing).

**Step 3: Implement** (append to `reminders.rs`):

```rust
/// Compile any recurring spec to a `cron::Schedule`.
fn compiled_schedule(spec: &ScheduleSpec) -> Result<Schedule> {
    let expr = match spec {
        ScheduleSpec::Once { .. } => bail!("Once has no recurring schedule"),
        ScheduleSpec::Pattern { freq, time, weekdays, day_of_month } => pattern_to_cron(
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
/// (wall-clock semantics); the cron crate + chrono-tz handle DST.
pub fn next_fire(spec: &ScheduleSpec, after: DateTime<Utc>, tz: Tz) -> Result<Option<DateTime<Utc>>> {
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
pub fn upcoming(spec: &ScheduleSpec, from: DateTime<Utc>, tz: Tz, n: usize) -> Result<Vec<DateTime<Utc>>> {
    match spec {
        ScheduleSpec::Once { due_at } => Ok((*due_at > from).then(|| vec![*due_at]).unwrap_or_default()),
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
        ScheduleSpec::Once { due_at } => format!("one-shot at {}", due_at.format("%Y-%m-%d %H:%M UTC")),
        ScheduleSpec::Pattern { freq, time, weekdays, day_of_month } => {
            let hm = time.format("%H:%M");
            match freq {
                Freq::Daily => format!("daily at {hm}"),
                Freq::Weekly => {
                    let days: Vec<String> = weekdays.iter().map(|w| format!("{w:?}")).collect();
                    format!("every {} at {hm}", days.join(", "))
                }
                Freq::Monthly => format!("monthly on day {day_of_month} at {hm}"),
            }
        }
        ScheduleSpec::Cron { expr } => format!("cron '{expr}'"),
    }
}
```

**Step 4: Run tests**

Run: `cargo test -p alexandria-engine reminders`
Expected: all PASS.

> **Contingency**: if the DST test fails because `cron`'s tz iteration doesn't preserve local wall time across the transition (verify actual behavior before changing the test!), do NOT weaken the assertion blindly. Options in order: (a) if crate semantics are defensible (e.g. fires at 10:00 EDT instead of 09:00 on transition day), document + adjust expectations; (b) replace `compiled_schedule` iteration for patterns with manual chrono-tz computation (`NaiveDate` iteration + `tz.from_local_datetime`). Record whichever choice you make in a code comment.

**Step 5: Commit**

```bash
git add crates/alexandria-engine/src/reminders.rs
git commit -m "feat(engine): next-fire computation, occurrence coalescing, DST-safe scheduling"
```

---

### Task 4: Storage — migration v006 + Reminder model

**Files:**

- Create: `crates/alexandria-storage/src/schema/v006_reminder.surql`
- Modify: `crates/alexandria-storage/src/schema/mod.rs` (MIGRATIONS const)
- Create: `crates/alexandria-storage/src/models/reminder.rs`
- Modify: `crates/alexandria-storage/src/models/mod.rs` (`mod reminder;` + `pub use reminder::Reminder;`)
- Modify: `crates/alexandria-storage/tests/migration_test.rs` (version assertions "5" → "6")

**Step 1: Write the failing test.** Append to `crates/alexandria-storage/tests/migration_test.rs`:

```rust
#[tokio::test]
async fn test_reminder_table_exists_after_migration() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();

    let mut result = db
        .inner()
        .query("CREATE reminder SET message = 'probe', schedule_kind = 'once', next_due_at = time::now()")
        .await
        .unwrap();
    result.check().unwrap();
}
```

Also update the two existing assertions: `assert_eq!(version, "5")` → `"6"` (and the comment above it).

**Step 2: Run to verify failure**

Run: `cargo test -p alexandria-storage --test migration_test`
Expected: FAIL — `reminder` table doesn't exist / version still "5".

**Step 3: Implement.**

`crates/alexandria-storage/src/schema/v006_reminder.surql`:

```sql
-- v006: Reminders

DEFINE TABLE reminder SCHEMAFULL;
DEFINE FIELD message          ON reminder TYPE string;
DEFINE FIELD target_project   ON reminder TYPE option<string>;
DEFINE FIELD prov_project     ON reminder TYPE option<string>;
DEFINE FIELD prov_session_id  ON reminder TYPE option<string>;
DEFINE FIELD note             ON reminder TYPE option<string>;
DEFINE FIELD schedule_kind    ON reminder TYPE string ASSERT $value IN ['once', 'pattern', 'cron'];
DEFINE FIELD due_at           ON reminder TYPE option<datetime>;
DEFINE FIELD freq             ON reminder TYPE option<string>;
DEFINE FIELD time_of_day      ON reminder TYPE option<string>;
DEFINE FIELD weekdays         ON reminder TYPE array<string> DEFAULT [];
DEFINE FIELD day_of_month     ON reminder TYPE option<int>;
DEFINE FIELD cron_expr        ON reminder TYPE option<string>;
DEFINE FIELD next_due_at      ON reminder TYPE option<datetime>;
DEFINE FIELD status           ON reminder TYPE string DEFAULT 'pending' ASSERT $value IN ['pending', 'delivered', 'cancelled'];
DEFINE FIELD created_at       ON reminder TYPE datetime DEFAULT time::now();
DEFINE FIELD cancelled_at     ON reminder TYPE option<datetime>;
DEFINE FIELD last_delivered_at ON reminder TYPE option<datetime>;
DEFINE FIELD delivered_count  ON reminder TYPE int DEFAULT 0;

DEFINE INDEX reminder_due_idx ON reminder FIELDS status, next_due_at;
```

In `schema/mod.rs` MIGRATIONS, after the v5 entry:

```rust
    (6, "reminder", include_str!("v006_reminder.surql")),
```

`crates/alexandria-storage/src/models/reminder.rs` (mirrors `session.rs` pattern — flat fields, no nested enum; the engine reconstructs `ScheduleSpec` from these):

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::{RecordId, SurrealValue};

/// A scheduled message. Flat storage shape; see `alexandria_engine::reminders::spec_from_reminder`
/// for reconstruction into a validated schedule.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Reminder {
    pub id: Option<RecordId>,
    pub message: String,
    /// None = global target; Some(name) = deliver only in that project (until escalation).
    pub target_project: Option<String>,
    // Provenance (display-only metadata)
    pub prov_project: Option<String>,
    pub prov_session_id: Option<String>,
    pub note: Option<String>,
    // Schedule: kind discriminator + kind-specific fields
    pub schedule_kind: String, // "once" | "pattern" | "cron"
    pub due_at: Option<DateTime<Utc>>,
    pub freq: Option<String>,
    pub time_of_day: Option<String>,
    pub weekdays: Vec<String>,
    pub day_of_month: Option<i64>,
    pub cron_expr: Option<String>,
    // State
    pub next_due_at: Option<DateTime<Utc>>,
    pub status: String, // "pending" | "delivered" | "cancelled"
    pub created_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub last_delivered_at: Option<DateTime<Utc>>,
    pub delivered_count: i64,
}
```

**Step 4: Run tests**

Run: `cargo test -p alexandria-storage --test migration_test`
Expected: PASS (all three tests).

**Step 5: Commit**

```bash
git add crates/alexandria-storage/
git commit -m "feat(storage): reminder table (migration v006) and model"
```

---

### Task 5: Storage — ReminderRepo

**Files:**

- Create: `crates/alexandria-storage/src/repos/reminder_repo.rs`
- Modify: `crates/alexandria-storage/src/repos/mod.rs` (`mod reminder_repo;` + `pub use reminder_repo::ReminderRepo;`)

**Step 1: Write the failing tests** — `#[cfg(test)] mod tests` inside `reminder_repo.rs` (follows `session_repo.rs` test pattern):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::Database;
    use chrono::{Duration, Utc};

    async fn setup() -> (Database, ReminderRepo<'static>) { /* see note below */ }

    fn probe(msg: &str, next_due_at: chrono::DateTime<Utc>, target: Option<&str>) -> NewReminder {
        NewReminder {
            message: msg.to_string(),
            target_project: target.map(str::to_string),
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: "once".to_string(),
            due_at: Some(next_due_at),
            freq: None,
            time_of_day: None,
            weekdays: vec![],
            day_of_month: None,
            cron_expr: None,
            next_due_at: Some(next_due_at),
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo.create(&probe("standup", Utc::now() + Duration::hours(1), None)).await.unwrap();
        assert!(id.starts_with("reminder:"));

        let fetched = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(fetched.message, "standup");
        assert_eq!(fetched.status, "pending");
        assert_eq!(fetched.delivered_count, 0);
    }

    #[tokio::test]
    async fn list_due_respects_time_and_status() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());
        let now = Utc::now();

        repo.create(&probe("past", now - Duration::minutes(5), None)).await.unwrap();
        repo.create(&probe("future", now + Duration::hours(1), None)).await.unwrap();
        let cancelled = repo.create(&probe("gone", now - Duration::minutes(5), None)).await.unwrap();
        repo.cancel(&cancelled).await.unwrap();

        let due = repo.list_due(now).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].message, "past");
    }

    #[tokio::test]
    async fn record_delivery_once_marks_delivered() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo.create(&probe("one", Utc::now() - Duration::minutes(1), None)).await.unwrap();
        repo.record_delivery(&id, None).await.unwrap();

        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "delivered");
        assert_eq!(r.delivered_count, 1);
        assert!(r.last_delivered_at.is_some());
    }

    #[tokio::test]
    async fn record_delivery_recurring_advances() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo.create(&probe("daily", Utc::now() - Duration::minutes(1), None)).await.unwrap();
        let next = Utc::now() + Duration::hours(24);
        repo.record_delivery(&id, Some(next)).await.unwrap();

        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "pending");
        assert_eq!(r.delivered_count, 1);
        let stored_next = r.next_due_at.unwrap();
        assert!((stored_next - next).num_seconds().abs() <= 1); // sub-second truncation tolerance
    }

    #[tokio::test]
    async fn cancel_hides_from_due_and_pending() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        let id = repo.create(&probe("x", Utc::now() - Duration::minutes(1), None)).await.unwrap();
        repo.cancel(&id).await.unwrap();

        assert!(repo.list_due(Utc::now()).await.unwrap().is_empty());
        let r = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(r.status, "cancelled");
        assert!(r.cancelled_at.is_some());
    }

    #[tokio::test]
    async fn list_filters_by_status_and_project() {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let repo = ReminderRepo::new(db.inner());

        repo.create(&probe("a", Utc::now(), Some("alexandria"))).await.unwrap();
        repo.create(&probe("b", Utc::now(), None)).await.unwrap();

        assert_eq!(repo.list(Some("pending"), None).await.unwrap().len(), 2);
        assert_eq!(repo.list(Some("pending"), Some("alexandria")).await.unwrap().len(), 1);
        assert_eq!(repo.list(Some("cancelled"), None).await.unwrap().len(), 0);
        assert_eq!(repo.list(None, None).await.unwrap().len(), 2);
    }
}
```

(Inline the `setup` helper away — the tests above each construct `db`/`repo` directly, matching `session_repo.rs` style. Drop the unused `setup` fn.)

**Step 2: Run to verify failure**

Run: `cargo test -p alexandria-engine --lib 2>/dev/null; cargo test -p alexandria-storage reminder`
Expected: compile failure (`ReminderRepo`/`NewReminder` missing).

**Step 3: Implement** `reminder_repo.rs`. Use `MemoryRepo`'s id-binding pattern (`type::record($id)` with a `RecordId` bound via `RecordId::parse_simple`) and `SessionRepo`'s query/take style:

```rust
use anyhow::Result;
use chrono::{DateTime, Utc};
use surrealdb::engine::any::Any;
use surrealdb::types::{RecordId, SurrealValue};
use surrealdb::Surreal;

use crate::models::Reminder;

/// Insert payload for `ReminderRepo::create` (no id/state fields).
#[derive(Debug, Clone)]
pub struct NewReminder {
    pub message: String,
    pub target_project: Option<String>,
    pub prov_project: Option<String>,
    pub prov_session_id: Option<String>,
    pub note: Option<String>,
    pub schedule_kind: String,
    pub due_at: Option<DateTime<Utc>>,
    pub freq: Option<String>,
    pub time_of_day: Option<String>,
    pub weekdays: Vec<String>,
    pub day_of_month: Option<i64>,
    pub cron_expr: Option<String>,
    pub next_due_at: Option<DateTime<Utc>>,
}

pub struct ReminderRepo<'a> {
    db: &'a Surreal<Any>,
}

impl<'a> ReminderRepo<'a> {
    pub fn new(db: &'a Surreal<Any>) -> Self {
        Self { db }
    }

    pub async fn create(&self, r: &NewReminder) -> Result<String> {
        let mut response = self
            .db
            .query(
                "CREATE reminder SET \
                 message = $message, target_project = $target_project, \
                 prov_project = $prov_project, prov_session_id = $prov_session_id, \
                 note = $note, schedule_kind = $schedule_kind, due_at = $due_at, \
                 freq = $freq, time_of_day = $time_of_day, weekdays = $weekdays, \
                 day_of_month = $day_of_month, cron_expr = $cron_expr, \
                 next_due_at = $next_due_at, status = 'pending', delivered_count = 0",
            )
            .bind(("message", r.message.clone()))
            .bind(("target_project", r.target_project.clone()))
            .bind(("prov_project", r.prov_project.clone()))
            .bind(("prov_session_id", r.prov_session_id.clone()))
            .bind(("note", r.note.clone()))
            .bind(("schedule_kind", r.schedule_kind.clone()))
            .bind(("due_at", r.due_at))
            .bind(("freq", r.freq.clone()))
            .bind(("time_of_day", r.time_of_day.clone()))
            .bind(("weekdays", r.weekdays.clone()))
            .bind(("day_of_month", r.day_of_month))
            .bind(("cron_expr", r.cron_expr.clone()))
            .bind(("next_due_at", r.next_due_at))
            .await?;
        let created: Option<Reminder> = response.take(0)?;
        let reminder = created.ok_or_else(|| anyhow::anyhow!("Failed to create reminder"))?;
        let id = reminder
            .id
            .ok_or_else(|| anyhow::anyhow!("Created reminder has no id"))?;
        Ok(crate::record_id_to_string(&id))
    }

    pub async fn get(&self, id: &str) -> Result<Option<Reminder>> {
        let rid = RecordId::parse_simple(id)?;
        let mut response = self
            .db
            .query("SELECT * FROM type::record($id)")
            .bind(("id", rid))
            .await?;
        let rows: Vec<Reminder> = response.take(0)?;
        Ok(rows.into_iter().next())
    }

    /// All pending reminders with next_due_at <= now, oldest first.
    pub async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Reminder>> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM reminder WHERE status = 'pending' AND next_due_at <= $now ORDER BY next_due_at ASC",
            )
            .bind(("now", now))
            .await?;
        Ok(response.take(0)?)
    }

    /// Status/project filters; both optional. Ordered by next_due_at.
    pub async fn list(
        &self,
        status: Option<&str>,
        target_project: Option<&str>,
    ) -> Result<Vec<Reminder>> {
        let mut clauses = Vec::new();
        let mut q;
        if let Some(s) = status {
            clauses.push("status = $status");
        }
        if let Some(p) = target_project {
            clauses.push("target_project = $target_project");
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        q = self
            .db
            .query(&format!("SELECT * FROM reminder{where_sql} ORDER BY next_due_at ASC"));
        if let Some(s) = status {
            q = q.bind(("status", s.to_string()));
        }
        if let Some(p) = target_project {
            q = q.bind(("target_project", p.to_string()));
        }
        let mut response = q.await?;
        Ok(response.take(0)?)
    }

    /// Consume a delivery. `new_next_due_at = None` → one-shot consumed
    /// (status = 'delivered'). `Some(next)` → recurring, stays pending with
    /// advanced next_due_at.
    pub async fn record_delivery(
        &self,
        id: &str,
        new_next_due_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let rid = RecordId::parse_simple(id)?;
        let (sql, has_next) = match new_next_due_at {
            Some(_) => (
                "UPDATE type::record($id) SET last_delivered_at = time::now(), \
                 delivered_count += 1, next_due_at = $next",
                true,
            ),
            None => (
                "UPDATE type::record($id) SET last_delivered_at = time::now(), \
                 delivered_count += 1, status = 'delivered'",
                false,
            ),
        };
        let mut q = self.db.query(sql).bind(("id", rid));
        if has_next {
            q = q.bind(("next", new_next_due_at));
        }
        q.await?.check()?;
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> Result<()> {
        let rid = RecordId::parse_simple(id)?;
        self.db
            .query("UPDATE type::record($id) SET status = 'cancelled', cancelled_at = time::now()")
            .bind(("id", rid))
            .await?
            .check()?;
        Ok(())
    }
}
```

> **Verify the `type::record($id)` SELECT pattern against `memory_repo.rs::get_fact`** — mirror exactly what that repo does for id lookups (it may use `SELECT * FROM type::record($id)` or a table-scoped variant). Do not invent a third pattern.

**Step 4: Run tests**

Run: `cargo test -p alexandria-storage reminder`
Expected: all PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-storage/src/repos/
git commit -m "feat(storage): ReminderRepo with due queries, delivery recording, cancel"
```

---

### Task 6: Engine — Reminder → ScheduleSpec conversion

**Files:**

- Modify: `crates/alexandria-engine/src/reminders.rs`

**Step 1: Write the failing test** (append):

```rust
    #[test]
    fn spec_from_reminder_roundtrips_all_kinds() {
        use alexandria_storage::models::Reminder;

        fn base() -> Reminder {
            Reminder {
                id: None, message: "m".into(), target_project: None,
                prov_project: None, prov_session_id: None, note: None,
                schedule_kind: "once".into(), due_at: None, freq: None,
                time_of_day: None, weekdays: vec![], day_of_month: None,
                cron_expr: None, next_due_at: None, status: "pending".into(),
                created_at: None, cancelled_at: None, last_delivered_at: None,
                delivered_count: 0,
            }
        }

        let mut once = base();
        once.due_at = Some(utc(2026, 9, 10, 15, 0));
        assert_eq!(
            spec_from_reminder(&once).unwrap(),
            ScheduleSpec::Once { due_at: utc(2026, 9, 10, 15, 0) }
        );

        let mut weekly = base();
        weekly.schedule_kind = "pattern".into();
        weekly.freq = Some("weekly".into());
        weekly.time_of_day = Some("09:00".into());
        weekly.weekdays = vec!["fri".into()];
        assert!(matches!(
            spec_from_reminder(&weekly).unwrap(),
            ScheduleSpec::Pattern { freq: Freq::Weekly, .. }
        ));

        let mut cr = base();
        cr.schedule_kind = "cron".into();
        cr.cron_expr = Some("0 0 9 * * 1-5".into());
        assert!(matches!(spec_from_reminder(&cr).unwrap(), ScheduleSpec::Cron { .. }));

        // Corrupt row → error, not panic
        let mut bad = base();
        bad.schedule_kind = "pattern".into();
        assert!(spec_from_reminder(&bad).is_err());
    }
```

**Step 2: Run** — `cargo test -p alexandria-engine reminders` — expect compile failure (`spec_from_reminder` missing).

**Step 3: Implement** (append to `reminders.rs`):

```rust
/// Reconstruct a validated spec from the flat storage row. Errors on corrupt
/// or inconsistent rows (never panics).
pub fn spec_from_reminder(r: &alexandria_storage::models::Reminder) -> Result<ScheduleSpec> {
    match r.schedule_kind.as_str() {
        "once" => Ok(ScheduleSpec::Once {
            due_at: r.due_at.context("once reminder missing due_at")?,
        }),
        "pattern" => {
            let freq = parse_freq(r.freq.as_deref().context("pattern missing freq")?)?;
            let time = parse_time_of_day(r.time_of_day.as_deref().context("pattern missing time_of_day")?)?;
            let mut weekdays = Vec::new();
            for w in &r.weekdays {
                weekdays.push(parse_weekday(w)?);
            }
            let day_of_month = r.day_of_month.unwrap_or(1);
            if day_of_month < 1 || day_of_month > 31 {
                bail!("day_of_month out of range: {day_of_month}");
            }
            Ok(ScheduleSpec::Pattern { freq, time, weekdays, day_of_month: day_of_month as u32 })
        }
        "cron" => {
            let expr = r.cron_expr.as_deref().context("cron reminder missing cron_expr")?;
            Ok(ScheduleSpec::Cron { expr: normalize_cron(expr)? })
        }
        other => bail!("unknown schedule_kind {other:?}"),
    }
}
```

**Step 4: Run** — `cargo test -p alexandria-engine reminders` — expect PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-engine/src/reminders.rs
git commit -m "feat(engine): reconstruct ScheduleSpec from stored Reminder rows"
```

---

### Task 7: Server config — `[reminders]` section

**Files:**

- Modify: `crates/alexandria/src/config.rs`

**Step 1: Write the failing tests** (append to `mod tests`):

```rust
    #[test]
    fn test_reminders_defaults() {
        let config = Config::default();
        assert_eq!(config.reminders.timezone, ""); // empty = system-local, resolved at startup
        assert_eq!(config.reminders.escalation_hours, 48);
    }

    #[test]
    fn test_reminders_from_toml() {
        let toml = r#"
            [reminders]
            timezone = "Europe/Stockholm"
            escalation_hours = 24
        "#;
        let config = Config::from_toml(toml).unwrap();
        assert_eq!(config.reminders.timezone, "Europe/Stockholm");
        assert_eq!(config.reminders.escalation_hours, 24);
    }

    #[test]
    #[serial]
    fn test_reminders_env_overrides() {
        std::env::set_var("ALEXANDRIA_REMINDERS_TIMEZONE", "America/New_York");
        std::env::set_var("ALEXANDRIA_REMINDERS_ESCALATION_HOURS", "12");

        let config = Config::load().unwrap();
        assert_eq!(config.reminders.timezone, "America/New_York");
        assert_eq!(config.reminders.escalation_hours, 12);

        std::env::remove_var("ALEXANDRIA_REMINDERS_TIMEZONE");
        std::env::remove_var("ALEXANDRIA_REMINDERS_ESCALATION_HOURS");
    }

    #[test]
    #[serial]
    fn test_reminders_env_invalid_hours() {
        std::env::set_var("ALEXANDRIA_REMINDERS_ESCALATION_HOURS", "soon");
        let result = Config::load();
        assert!(result.is_err());
        std::env::remove_var("ALEXANDRIA_REMINDERS_ESCALATION_HOURS");
    }
```

**Step 2: Run** — `cargo test -p alexandria config` — expect compile failure (`reminders` field missing).

**Step 3: Implement.** In `config.rs`:

Add to `Config`:

```rust
    pub reminders: RemindersConfig,
```

New section struct (place after `ClusterConfig`, matching the file's ordering style):

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RemindersConfig {
    /// IANA timezone for naive datetime input and pattern/cron evaluation.
    /// Empty string = system-local (resolved via iana-time-zone at startup).
    pub timezone: String,
    /// Project-targeted reminders escalate to global delivery after being
    /// overdue this long. Default 48.
    pub escalation_hours: u64,
}

impl Default for RemindersConfig {
    fn default() -> Self {
        Self {
            timezone: String::new(),
            escalation_hours: 48,
        }
    }
}
```

In `Config::load()`, after the `ALEXANDRIA_EMBEDDING_DEVICE` override block:

```rust
        if let Ok(tz) = std::env::var("ALEXANDRIA_REMINDERS_TIMEZONE") {
            config.reminders.timezone = tz;
        }
        if let Ok(h) = std::env::var("ALEXANDRIA_REMINDERS_ESCALATION_HOURS") {
            config.reminders.escalation_hours = h.parse().map_err(|e| {
                anyhow::anyhow!("invalid ALEXANDRIA_REMINDERS_ESCALATION_HOURS `{h}`: {e}")
            })?;
        }
```

**Step 4: Run** — `cargo test -p alexandria config` — expect PASS.

**Step 5: Commit**

```bash
git add crates/alexandria/src/config.rs
git commit -m "feat(config): [reminders] section (timezone, escalation_hours) with env overrides"
```

---

### Task 8: MCP — params, server settings, main.rs wiring

**Files:**

- Create: `crates/alexandria-mcp/src/tools/set_reminder.rs`
- Create: `crates/alexandria-mcp/src/tools/check_reminders.rs`
- Create: `crates/alexandria-mcp/src/tools/list_reminders.rs`
- Create: `crates/alexandria-mcp/src/tools/cancel_reminder.rs`
- Modify: `crates/alexandria-mcp/src/tools/mod.rs`
- Modify: `crates/alexandria-mcp/src/server.rs` (struct field + builder)
- Modify: `crates/alexandria/src/main.rs` (wiring)

**Step 1: Params structs.**

`tools/set_reminder.rs`:

```rust
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ReminderPatternParams {
    #[schemars(description = "Recurrence frequency: 'daily', 'weekly', or 'monthly'")]
    pub freq: String,
    #[schemars(description = "Wall-clock time in the server's configured timezone, HH:MM (24h), e.g. '09:00'")]
    pub time: String,
    #[schemars(description = "For freq=weekly: list of weekday names, e.g. ['mon','fri']. Omit for daily/monthly.")]
    pub weekdays: Option<Vec<String>>,
    #[schemars(description = "For freq=monthly: day of month 1-31. Months without that day are skipped. Omit for daily/weekly.")]
    pub day_of_month: Option<u32>,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct SetReminderParams {
    #[schemars(description = "The reminder text to deliver. Write it standalone — it will be shown in a future session without today's conversation.")]
    pub message: String,
    #[schemars(description = "One-shot: ISO-8601 datetime. Explicit offsets honored; naive timestamps are interpreted in the server timezone. Exactly one of due_at/pattern/cron must be given.")]
    pub due_at: Option<String>,
    #[schemars(description = "Recurring named pattern: {freq: daily|weekly|monthly, time: 'HH:MM', weekdays?, day_of_month?}. Exactly one of due_at/pattern/cron must be given.")]
    pub pattern: Option<ReminderPatternParams>,
    #[schemars(description = "Recurring cron escape hatch: standard 5-field cron (min hour dom mon dow), evaluated in the server timezone. Exactly one of due_at/pattern/cron must be given. Prefer pattern unless you need cron-only expressiveness.")]
    pub cron: Option<String>,
    #[schemars(description = "Optional project name to target delivery (e.g. 'alexandria'). Omit for a global reminder. Project reminders escalate to global delivery if overdue too long, so they are never silently lost.")]
    pub target_project: Option<String>,
    #[schemars(description = "Optional provenance: project this reminder was set in (display metadata only, does not affect delivery)")]
    pub prov_project: Option<String>,
    #[schemars(description = "Optional provenance: session ID this reminder was set in (display metadata only)")]
    pub session_id: Option<String>,
    #[schemars(description = "Optional extra context shown alongside the reminder at delivery time")]
    pub note: Option<String>,
}
```

`tools/check_reminders.rs`:

```rust
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct CheckRemindersParams {
    #[schemars(description = "Current project name (e.g. git repo dir name). Project-targeted reminders matching this are delivered; global and escalated reminders are delivered regardless.")]
    pub project: Option<String>,
}
```

`tools/list_reminders.rs`:

```rust
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListRemindersParams {
    #[schemars(description = "Filter by status: 'pending' (default), 'delivered', 'cancelled', or 'all'")]
    pub status: Option<String>,
    #[schemars(description = "Optional: only reminders targeting this project")]
    pub target_project: Option<String>,
}
```

`tools/cancel_reminder.rs`:

```rust
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct CancelReminderParams {
    #[schemars(description = "Reminder ID, e.g. 'reminder:abc123' (from set/list output)")]
    pub id: String,
}
```

`tools/mod.rs` — add the four `mod` declarations (alphabetical) and re-exports:

```rust
pub use cancel_reminder::CancelReminderParams;
pub use check_reminders::CheckRemindersParams;
pub use list_reminders::ListRemindersParams;
pub use set_reminder::{ReminderPatternParams, SetReminderParams};
```

**Step 2: Server settings.** In `crates/alexandria-mcp/src/server.rs`:

Add near the top (after imports):

```rust
/// Reminder delivery settings resolved from server config at startup.
#[derive(Debug, Clone)]
pub struct RemindersSettings {
    pub tz: chrono_tz::Tz,
    pub escalation_hours: u64,
}

impl Default for RemindersSettings {
    fn default() -> Self {
        Self {
            tz: chrono_tz::Tz::UTC,
            escalation_hours: 48,
        }
    }
}
```

Add field to `AlexandriaServer`: `pub reminders: RemindersSettings,` — initialize `RemindersSettings::default()` in `new()`, and add a builder next to the existing `with_*` methods:

```rust
    pub fn with_reminders_config(mut self, settings: RemindersSettings) -> Self {
        self.reminders = settings;
        self
    }
```

**Step 3: main.rs wiring.** In `crates/alexandria/src/main.rs`, in the "4. Create MCP server" block, extend the builder chain:

```rust
    // Resolve reminders timezone: config value, else system-local, else UTC
    let tz_name = if config.reminders.timezone.is_empty() {
        iana_time_zone::get_timezone().unwrap_or_else(|e| {
            tracing::warn!("Could not detect system timezone ({e}); using UTC for reminders");
            "UTC".to_string()
        })
    } else {
        config.reminders.timezone.clone()
    };
    let tz: chrono_tz::Tz = tz_name.parse().map_err(|e| {
        anyhow::anyhow!("invalid [reminders].timezone `{tz_name}` (expected IANA name like 'Europe/Stockholm'): {e}")
    })?;
    tracing::info!("Reminders timezone: {tz}, escalation: {}h", config.reminders.escalation_hours);

    let server = AlexandriaServer::new(/* unchanged args */)
        .with_activation_config(activation_config)
        .with_activation_top_n(config.activation.top_n)
        .with_retrieve_min_similarity(config.retrieve.min_similarity)
        .with_reminders_config(alexandria_mcp::server::RemindersSettings {
            tz,
            escalation_hours: config.reminders.escalation_hours,
        });
```

**Step 4: Verify** — `just check` → compiles.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/ crates/alexandria/src/main.rs
git commit -m "feat(mcp): reminder tool params, RemindersSettings, main.rs wiring"
```

---

### Task 9: MCP — `set_reminder`

**Files:**

- Modify: `crates/alexandria-mcp/src/server.rs`
- Create: `crates/alexandria/tests/reminders_test.rs`
- Modify: `crates/alexandria/Cargo.toml` (dev-dep `async-trait = "0.1"`)

**Step 1: Write the failing test file** `crates/alexandria/tests/reminders_test.rs`:

```rust
use std::sync::Arc;

use alexandria_mcp::server::{AlexandriaServer, RemindersSettings};
use alexandria_mcp::tools::SetReminderParams;
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::{schema, Database};
use anyhow::Result;
use async_trait::async_trait;

/// Reminders never embed, so a no-op provider keeps these tests fast
/// (no model download — unlike integration_test.rs).
struct StubEmbedding;

#[async_trait]
impl EmbeddingProvider for StubEmbedding {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0, 0.1]).collect())
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn model_id(&self) -> &str {
        "stub"
    }
}

fn tz() -> chrono_tz::Tz {
    "UTC".parse().unwrap()
}

async fn setup() -> AlexandriaServer {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();
    AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0)
        .with_reminders_config(RemindersSettings {
            tz: tz(),
            escalation_hours: 48,
        })
}

fn once_params(msg: &str, due: &str) -> SetReminderParams {
    SetReminderParams {
        message: msg.to_string(),
        due_at: Some(due.to_string()),
        pattern: None,
        cron: None,
        target_project: None,
        prov_project: None,
        session_id: None,
        note: None,
    }
}

#[tokio::test]
async fn set_reminder_once_ok() {
    let server = setup().await;
    let result = server
        .do_set_reminder(once_params("check deploy", "2030-01-01T12:00:00Z"))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["status"], "ok");
    assert!(v["id"].as_str().unwrap().starts_with("reminder:"));
    assert_eq!(v["next_due_at"], "2030-01-01T12:00:00Z");
    assert_eq!(v["next_fire_preview"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn set_reminder_rejects_multiple_schedules() {
    let server = setup().await;
    let mut p = once_params("x", "2030-01-01T12:00:00Z");
    p.cron = Some("0 9 * * *".to_string());
    let err = server.do_set_reminder(p).await.unwrap_err().to_string();
    assert!(err.contains("exactly one"), "unexpected error: {err}");
}

#[tokio::test]
async fn set_reminder_rejects_bad_cron() {
    let server = setup().await;
    let mut p = once_params("x", "2030-01-01T12:00:00Z");
    p.due_at = None;
    p.cron = Some("61 99 * * *".to_string());
    assert!(server.do_set_reminder(p).await.is_err());
}

#[tokio::test]
async fn set_reminder_pattern_weekly_previews_three_fires() {
    let server = setup().await;
    let p = SetReminderParams {
        message: "standup".to_string(),
        due_at: None,
        pattern: Some(alexandria_mcp::tools::ReminderPatternParams {
            freq: "weekly".to_string(),
            time: "09:00".to_string(),
            weekdays: Some(vec!["fri".to_string()]),
            day_of_month: None,
        }),
        cron: None,
        target_project: Some("alexandria".to_string()),
        prov_project: None,
        session_id: None,
        note: None,
    };
    let result = server.do_set_reminder(p).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["next_fire_preview"].as_array().unwrap().len(), 3);
    assert_eq!(v["schedule"], "every Friday at 09:00");
}

#[tokio::test]
async fn set_reminder_past_once_warns_but_stores() {
    let server = setup().await;
    let result = server
        .do_set_reminder(once_params("forgotten thing", "2020-01-01T12:00:00Z"))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["status"], "ok");
    assert!(v["warning"].as_str().unwrap().contains("past"));
}
```

**Step 2: Run** — `cargo test -p alexandria --test reminders_test` — expect compile failure (`do_set_reminder` missing).

**Step 3: Implement.** In `server.rs`:

Tool entry inside the `#[tool_router]` impl block (after `finalize_session`, keeping style):

```rust
    #[tool(
        description = "Schedule a reminder to be delivered on a future interaction — one-shot (due_at) or recurring (pattern/cron). Use this when the user asks to be reminded of something later, when they say 'remind me', 'don't let me forget', or when a follow-up action will be needed at a specific time ('check the deploy at 3pm', 'ping me about this tomorrow morning'). Reminders are delivered to both the agent context and the user on the next interaction after they come due; project-targeted reminders escalate to global delivery if they stay overdue, so they are never silently lost. The response includes the next fire times — confirm them with the user when the schedule was parsed from natural language."
    )]
    async fn set_reminder(&self, Parameters(params): Parameters<SetReminderParams>) -> String {
        match self.do_set_reminder(params).await {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }
```

Add `SetReminderParams` (and later the others) to the `use crate::tools::{...}` import list.

Implementation in the `// Implementation details` impl block:

```rust
    pub async fn do_set_reminder(&self, params: SetReminderParams) -> anyhow::Result<String> {
        use alexandria_engine::reminders as sched;
        use alexandria_storage::repos::{NewReminder, ReminderRepo};

        let tz = self.reminders.tz;
        let now = chrono::Utc::now();

        // Exactly one schedule kind
        let kinds = [
            params.due_at.is_some(),
            params.pattern.is_some(),
            params.cron.is_some(),
        ];
        if kinds.iter().filter(|b| **b).count() != 1 {
            anyhow::bail!("provide exactly one of due_at, pattern, or cron");
        }

        let (spec, kind, row_fields): (
            sched::ScheduleSpec,
            &str,
            (
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
                Option<String>,
                Vec<String>,
                Option<i64>,
                Option<String>,
            ),
        ) = if let Some(due) = params.due_at.as_deref() {
            let due_at = sched::parse_datetime(due, tz)?;
            (
                sched::ScheduleSpec::Once { due_at },
                "once",
                (Some(due_at), None, None, vec![], None, None),
            )
        } else if let Some(pat) = params.pattern.as_ref() {
            let freq = sched::parse_freq(&pat.freq)?;
            let time = sched::parse_time_of_day(&pat.time)?;
            let mut weekdays = Vec::new();
            if let Some(ws) = &pat.weekdays {
                for w in ws {
                    weekdays.push(sched::parse_weekday(w)?);
                }
            }
            let dom = pat.day_of_month.unwrap_or(1);
            // Validate combination before storing
            sched::pattern_to_cron(freq, time, &weekdays, (freq == sched::Freq::Monthly).then_some(dom))?;
            (
                sched::ScheduleSpec::Pattern { freq, time, weekdays, day_of_month: dom },
                "pattern",
                (
                    None,
                    Some(pat.freq.to_ascii_lowercase()),
                    Some(format!("{:02}:{:02}", {
                        use chrono::Timelike;
                        time.hour()
                    }, {
                        use chrono::Timelike;
                        time.minute()
                    })),
                    pat.weekdays.clone().unwrap_or_default().iter().map(|w| w.to_ascii_lowercase()).collect(),
                    (freq == sched::Freq::Monthly).then_some(dom as i64),
                    None,
                ),
            )
        } else {
            let expr = sched::normalize_cron(params.cron.as_deref().unwrap())?;
            (
                sched::ScheduleSpec::Cron { expr: expr.clone() },
                "cron",
                (None, None, None, vec![], None, Some(expr)),
            )
        };

        // Compute next fire + preview (recurring evaluated in configured tz)
        let next = sched::next_fire(&spec, now, tz)?;
        let preview = sched::upcoming(&spec, now, tz, 3)?;
        let warning = match (&spec, next) {
            (sched::ScheduleSpec::Once { due_at }, None) => Some(format!(
                "due_at {} is in the past; this reminder will fire on the very next check",
                due_at.to_rfc3339()
            )),
            _ => None,
        };
        // Once-in-past still stores with next_due_at = due_at so list_due catches it
        let next_due_at = match &spec {
            sched::ScheduleSpec::Once { due_at } => Some(*due_at),
            _ => next,
        };

        let repo = ReminderRepo::new(self.db.inner());
        let id = repo
            .create(&NewReminder {
                message: params.message.clone(),
                target_project: params.target_project.clone(),
                prov_project: params.prov_project.clone(),
                prov_session_id: params.session_id.clone(),
                note: params.note.clone(),
                schedule_kind: kind.to_string(),
                due_at: row_fields.0,
                freq: row_fields.1,
                time_of_day: row_fields.2,
                weekdays: row_fields.3,
                day_of_month: row_fields.4,
                cron_expr: row_fields.5,
                next_due_at,
            })
            .await?;

        let mut out = serde_json::json!({
            "status": "ok",
            "id": id,
            "schedule": sched::human_readable(&spec),
            "next_due_at": next_due_at.map(|d| d.to_rfc3339()),
            "next_fire_preview": preview.iter().map(|d| d.to_rfc3339()).collect::<Vec<_>>(),
            "timezone": tz.name().to_string(),
        });
        if let Some(w) = warning {
            out["warning"] = serde_json::Value::String(w);
        }
        Ok(out.to_string())
    }
```

> The tuple-of-row-fields shape above is deliberately mechanical; if clippy complains about complexity (`type_complexity`), extract a small local struct `RowFields { due_at, freq, time_of_day, weekdays, day_of_month, cron_expr }` — cleaner anyway.

**Step 4: Run** — `cargo test -p alexandria --test reminders_test` — expect PASS (5 tests).

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/server.rs crates/alexandria/tests/reminders_test.rs crates/alexandria/Cargo.toml
git commit -m "feat(mcp): set_reminder with set-time validation and next-fire preview"
```

---

### Task 10: MCP — `check_reminders` (delivery + consumption)

**Files:**

- Modify: `crates/alexandria-mcp/src/server.rs`
- Modify: `crates/alexandria/tests/reminders_test.rs`

**Step 1: Write the failing tests** (append to `reminders_test.rs`):

```rust
use alexandria_mcp::tools::CheckRemindersParams;

fn check(project: Option<&str>) -> CheckRemindersParams {
    CheckRemindersParams { project: project.map(str::to_string) }
}

#[tokio::test]
async fn check_delivers_global_and_consumes_once() {
    let server = setup().await;
    server
        .do_set_reminder(once_params("pay rent", "2020-01-01T12:00:00Z"))
        .await
        .unwrap();

    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["message"], "pay rent");

    // second check: consumed, nothing due
    let out2: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out2["count"], 0);
}

#[tokio::test]
async fn check_project_targeting_and_escalation() {
    let server = setup().await;
    let mut p = once_params("renew cert", "2020-01-01T12:00:00Z");
    p.target_project = Some("infra".to_string());
    server.do_set_reminder(p).await.unwrap();

    // wrong project → not delivered
    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(Some("other"))).await.unwrap()).unwrap();
    assert_eq!(out["count"], 0);

    // right project → delivered
    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(Some("infra"))).await.unwrap()).unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["escalated"], false);
}

#[tokio::test]
async fn check_escalates_long_overdue_project_reminder() {
    // escalation_hours = 0 in this server → any overdue project reminder escalates
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();
    let server = AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0)
        .with_reminders_config(RemindersSettings { tz: tz(), escalation_hours: 0 });

    let mut p = once_params("stale project thing", "2020-01-01T12:00:00Z");
    p.target_project = Some("abandoned".to_string());
    server.do_set_reminder(p).await.unwrap();

    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(Some("elsewhere"))).await.unwrap()).unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["escalated"], true);
}

#[tokio::test]
async fn check_recurring_advances_and_coalesces() {
    let server = setup().await;
    let p = SetReminderParams {
        message: "hourly check".to_string(),
        due_at: None,
        pattern: None,
        // every minute — guarantees multiple occurrences pass during the test
        cron: Some("* * * * *".to_string()),
        target_project: None,
        prov_project: None,
        session_id: None,
        note: None,
    };
    server.do_set_reminder(p).await.unwrap();

    // first check: due (next_fire at set time was ≤1min ahead — sleep past it)
    tokio::time::sleep(std::time::Duration::from_secs(65)).await;
    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["recurring"], true);
    // ≥1 occurrence elapsed since set; missed = occurrences between stored
    // next_due_at and now, excluding the delivered one
    assert!(out["delivered"][0]["missed_occurrences"].as_u64().unwrap() >= 1);

    // next_due_at advanced into the future: immediate recheck delivers nothing
    let out2: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out2["count"], 0);
}
```

> The 65s sleep makes this test slow (~70s). Mark it `#[ignore]` by default and run explicitly in Step 4 (`-- --ignored check_recurring`), OR keep it un-ignored if the suite's total time is acceptable. **Decision: `#[ignore]`** — CI stays fast; the plan's final task runs ignored tests once.

**Step 2: Run** — `cargo test -p alexandria --test reminders_test` — expect compile failure (`do_check_reminders` missing).

**Step 3: Implement.** Tool entry (after `set_reminder`):

```rust
    #[tool(
        description = "Check for reminders that have come due and consume them. Client integrations call this automatically at the start of each interaction; you generally don't need to call it manually unless the user asks 'any reminders?'. Delivery is best-effort-once: calling this marks returned reminders as delivered (recurring ones advance to their next occurrence, coalescing any missed fires into a missed_occurrences count)."
    )]
    async fn check_reminders(&self, Parameters(params): Parameters<CheckRemindersParams>) -> String {
        match self.do_check_reminders(params).await {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }
```

Implementation:

```rust
    pub async fn do_check_reminders(&self, params: CheckRemindersParams) -> anyhow::Result<String> {
        use alexandria_engine::reminders as sched;
        use alexandria_storage::repos::ReminderRepo;

        let tz = self.reminders.tz;
        let now = chrono::Utc::now();
        let escalation = chrono::Duration::hours(self.reminders.escalation_hours as i64);

        let repo = ReminderRepo::new(self.db.inner());
        let due = repo.list_due(now).await?;

        let mut delivered = Vec::new();
        for r in due {
            // Targeting: global always; project on match; escalated when overdue long enough
            let escalated = match &r.target_project {
                None => false,
                Some(target) => {
                    let matches_ctx = params.project.as_deref() == Some(target.as_str());
                    if matches_ctx {
                        false
                    } else {
                        let overdue_by = now - r.next_due_at.unwrap_or(now);
                        if overdue_by < escalation {
                            continue; // held for a matching context
                        }
                        true
                    }
                }
            };

            let spec = sched::spec_from_reminder(&r)?;
            let recurring = !matches!(spec, sched::ScheduleSpec::Once { .. });

            let missed = if recurring {
                if let Some(prev_next) = r.next_due_at {
                    sched::occurrences_between(&spec, prev_next, now, tz)?
                } else {
                    0
                }
            } else {
                0
            };

            // Advance (recurring) or consume (one-shot)
            let new_next = if recurring {
                sched::next_fire(&spec, now, tz)?
            } else {
                None
            };
            let id = r
                .id
                .as_ref()
                .map(crate::record_id_to_string)
                .unwrap_or_default();
            repo.record_delivery(&id, new_next).await?;

            delivered.push(serde_json::json!({
                "id": id,
                "message": r.message,
                "target": match &r.target_project {
                    Some(p) => format!("project:{p}"),
                    None => "global".to_string(),
                },
                "escalated": escalated,
                "recurring": recurring,
                "missed_occurrences": missed,
                "due_at": r.next_due_at.map(|d| d.to_rfc3339()),
                "schedule": sched::human_readable(&spec),
                "note": r.note,
                "provenance": {
                    "project": r.prov_project,
                    "session_id": r.prov_session_id,
                },
                "next_due_at": new_next.map(|d| d.to_rfc3339()),
            }));
        }

        Ok(serde_json::json!({
            "count": delivered.len(),
            "delivered": delivered,
        })
        .to_string())
    }
```

**Step 4: Run**

Run: `cargo test -p alexandria --test reminders_test && cargo test -p alexandria --test reminders_test -- --ignored check_recurring`
Expected: all PASS (ignored one takes ~70s).

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/server.rs crates/alexandria/tests/reminders_test.rs
git commit -m "feat(mcp): check_reminders with targeting, escalation, coalescing consumption"
```

---

### Task 11: MCP — `list_reminders` + `cancel_reminder`

**Files:**

- Modify: `crates/alexandria-mcp/src/server.rs`
- Modify: `crates/alexandria/tests/reminders_test.rs`

**Step 1: Failing tests** (append):

```rust
use alexandria_mcp::tools::{CancelReminderParams, ListRemindersParams};

#[tokio::test]
async fn list_and_cancel_roundtrip() {
    let server = setup().await;
    let out = server
        .do_set_reminder(once_params("water plants", "2030-01-01T12:00:00Z"))
        .await
        .unwrap();
    let id: String = serde_json::from_str::<serde_json::Value>(&out).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // default status filter = pending
    let list: serde_json::Value = serde_json::from_str(
        &server
            .do_list_reminders(ListRemindersParams { status: None, target_project: None })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list["count"], 1);
    assert_eq!(list["reminders"][0]["message"], "water plants");
    assert_eq!(list["reminders"][0]["schedule"], "one-shot at 2030-01-01 12:00 UTC");

    // cancel
    let cancel: serde_json::Value = serde_json::from_str(
        &server
            .do_cancel_reminder(CancelReminderParams { id: id.clone() })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cancel["status"], "ok");

    // gone from pending, visible under cancelled
    let list2: serde_json::Value = serde_json::from_str(
        &server
            .do_list_reminders(ListRemindersParams { status: None, target_project: None })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list2["count"], 0);
    let list3: serde_json::Value = serde_json::from_str(
        &server
            .do_list_reminders(ListRemindersParams {
                status: Some("cancelled".to_string()),
                target_project: None,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list3["count"], 1);

    // cancel unknown id → error json, not panic
    let err = server
        .do_cancel_reminder(CancelReminderParams { id: "reminder:nonexistent".to_string() })
        .await;
    assert!(err.is_err());
}
```

**Step 2: Run** — expect compile failure.

**Step 3: Implement.** Tool entries:

```rust
    #[tool(
        description = "List reminders (pending by default; filter by status 'pending'|'delivered'|'cancelled'|'all' and/or target project). Use when the user asks what reminders exist, or to find an ID before cancelling."
    )]
    async fn list_reminders(&self, Parameters(params): Parameters<ListRemindersParams>) -> String {
        match self.do_list_reminders(params).await {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }

    #[tool(
        description = "Cancel a reminder by ID (soft-cancel; it stays listed under status=cancelled). Use when the user says a reminder is no longer needed, or after a reminder was delivered and the follow-up is done — for recurring reminders the user no longer wants."
    )]
    async fn cancel_reminder(&self, Parameters(params): Parameters<CancelReminderParams>) -> String {
        match self.do_cancel_reminder(params).await {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({ "status": "error", "message": e.to_string() }).to_string()
            }
        }
    }
```

Implementations:

```rust
    pub async fn do_list_reminders(&self, params: ListRemindersParams) -> anyhow::Result<String> {
        use alexandria_engine::reminders as sched;
        use alexandria_storage::repos::ReminderRepo;

        let status = match params.status.as_deref() {
            None | Some("pending") => Some("pending"),
            Some("all") => None,
            s @ Some(_) => s,
        };
        let repo = ReminderRepo::new(self.db.inner());
        let rows = repo.list(status, params.target_project.as_deref()).await?;

        let reminders: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let spec = sched::spec_from_reminder(r);
                serde_json::json!({
                    "id": r.id.as_ref().map(crate::record_id_to_string).unwrap_or_default(),
                    "message": r.message,
                    "target": match &r.target_project {
                        Some(p) => format!("project:{p}"),
                        None => "global".to_string(),
                    },
                    "status": r.status,
                    "schedule": spec.as_ref().map(sched::human_readable).unwrap_or_else(|e| format!("<invalid schedule: {e}>")),
                    "next_due_at": r.next_due_at.map(|d| d.to_rfc3339()),
                    "delivered_count": r.delivered_count,
                    "last_delivered_at": r.last_delivered_at.map(|d| d.to_rfc3339()),
                    "note": r.note,
                    "created_at": r.created_at.map(|d| d.to_rfc3339()),
                })
            })
            .collect();

        Ok(serde_json::json!({ "count": reminders.len(), "reminders": reminders }).to_string())
    }

    pub async fn do_cancel_reminder(&self, params: CancelReminderParams) -> anyhow::Result<String> {
        use alexandria_storage::repos::ReminderRepo;

        let repo = ReminderRepo::new(self.db.inner());
        let existing = repo.get(&params.id).await?;
        if existing.is_none() {
            anyhow::bail!("Reminder not found: {}", params.id);
        }
        repo.cancel(&params.id).await?;
        Ok(serde_json::json!({ "status": "ok", "id": params.id }).to_string())
    }
```

Add `CancelReminderParams, CheckRemindersParams, ListRemindersParams, SetReminderParams` to the `use crate::tools::{...}` import.

**Step 4: Run** — `cargo test -p alexandria --test reminders_test` — expect PASS.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/server.rs crates/alexandria/tests/reminders_test.rs
git commit -m "feat(mcp): list_reminders and cancel_reminder"
```

---

### Task 12: MCP — read-only `due_reminders` piggyback on retrieve/recall

**Files:**

- Modify: `crates/alexandria-mcp/src/server.rs` (`do_retrieve_memories`, `do_recall`)
- Modify: `crates/alexandria/tests/reminders_test.rs`

**Step 1: Failing test** (append):

```rust
#[tokio::test]
async fn piggyback_lists_due_reminders_without_consuming() {
    let server = setup().await;
    server
        .do_set_reminder(once_params("nag me", "2020-01-01T12:00:00Z"))
        .await
        .unwrap();
    server
        .do_store_memory(alexandria_mcp::tools::StoreMemoryParams {
            content: "OAuth tokens expire after 7 days".to_string(),
            tags: None,
            session_id: None,
        })
        .await
        .unwrap();

    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_retrieve_memories(alexandria_mcp::tools::RetrieveMemoriesParams {
                query: "token expiration".to_string(),
                limit: Some(5),
                session_id: None,
            })
            .await
            .unwrap()
            .to_string(),
    )
    .unwrap();
    assert_eq!(out["due_reminders"].as_array().unwrap().len(), 1);
    assert_eq!(out["due_reminders"][0]["message"], "nag me");

    // NOT consumed — check_reminders still delivers it
    let chk: serde_json::Value = serde_json::from_str(
        &server.do_check_reminders(check(None)).await.unwrap(),
    )
    .unwrap();
    assert_eq!(chk["count"], 1);
}
```

**Step 2: Run** — expect FAIL (`due_reminders` absent).

**Step 3: Implement.** Private helper on `AlexandriaServer`:

```rust
    /// Read-only due-reminder summary for piggybacking onto memory responses.
    /// Never consumes. Capped to keep noise bounded.
    async fn due_reminders_summary(&self, cap: usize) -> Vec<serde_json::Value> {
        use alexandria_storage::repos::ReminderRepo;
        let repo = ReminderRepo::new(self.db.inner());
        match repo.list_due(chrono::Utc::now()).await {
            Ok(rows) => rows
                .into_iter()
                .take(cap)
                .map(|r| {
                    serde_json::json!({
                        "id": r.id.as_ref().map(crate::record_id_to_string).unwrap_or_default(),
                        "message": r.message,
                        "target": match &r.target_project {
                            Some(p) => format!("project:{p}"),
                            None => "global".to_string(),
                        },
                        "due_at": r.next_due_at.map(|d| d.to_rfc3339()),
                    })
                })
                .collect(),
            Err(e) => {
                tracing::warn!("due_reminders piggyback failed: {e}");
                Vec::new() // fail-open: never break retrieval over reminders
            }
        }
    }
```

In `do_retrieve_memories`:

- the `facts.is_empty()` early return becomes `Ok(serde_json::json!({ "results": [], "due_reminders": self.due_reminders_summary(5).await }))`
- the final response gains `"due_reminders": self.due_reminders_summary(5).await`

In `do_recall`: add `"due_reminders": self.due_reminders_summary(5).await` to **both** the focused and broad response `json!` objects.

**Step 4: Run** — `cargo test -p alexandria --test reminders_test` — expect PASS. Also run the full existing suite to catch response-shape regressions: `just test` (recall/retrieve tests assert on specific fields — adding a field shouldn't break `serde_json` assertions, but verify).

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/server.rs crates/alexandria/tests/reminders_test.rs
git commit -m "feat(mcp): piggyback read-only due_reminders onto retrieve/recall responses"
```

---

### Task 13: Instructions, skill, docs

**Files:**

- Modify: `crates/alexandria-mcp/src/server.rs` (`#[tool_handler(instructions = ...)]`)
- Modify: `contrib/pi/skills/alexandria-memory/SKILL.md`
- Modify: `README.md` (root — read it first; add reminders to the tools overview + a `[reminders]` config row)
- Modify: `CLAUDE.md` (Non-Obvious Patterns)

**Step 1: Instructions.** Append a new paragraph to the `#[tool_handler]` instructions string (before the final sentence block, matching the existing `\n\n\` line-continuation style):

```
Reminders: use set_reminder when the user asks to be reminded of something later or a follow-up will be needed at a specific time — confirm the parsed schedule against the returned next_fire_preview. Due reminders are delivered automatically at the start of interactions (and surfaced read-only in retrieve/recall responses); check_reminders consumes them, so only call it directly when asked 'any reminders due?'. Use list_reminders to review and cancel_reminder when a reminder is no longer needed.
```

**Step 2: SKILL.md.** Read `contrib/pi/skills/alexandria-memory/SKILL.md`, then add a "Reminders" section in the same voice: when to set (explicit "remind me", time-bound follow-ups), schedule kinds with one example each, targeting semantics (global default; project for repo-bound items), and that delivery is automatic.

**Step 3: README + CLAUDE.md.** Add to CLAUDE.md "Non-Obvious Patterns":

```
- Reminders are evaluated purely at query time — there is NO background timer. Due-ness = `status='pending' AND next_due_at <= now()`; consumption advances recurring `next_due_at` to the first occurrence after now, coalescing missed fires into `missed_occurrences`. Works identically in stdio and HTTP mode.
- `check_reminders` consumes (best-effort-once); the `due_reminders` piggyback on retrieve/recall is read-only and never advances state.
- Naive datetimes in `set_reminder` are interpreted in `[reminders].timezone` (default: system-local via iana-time-zone); explicit ISO offsets always win.
```

Update root README tools list/config table to match (read the existing format first).

**Step 4: Verify** — `just check && just lint`.

**Step 5: Commit**

```bash
git add crates/alexandria-mcp/src/server.rs contrib/pi/skills/alexandria-memory/SKILL.md README.md CLAUDE.md
git commit -m "docs: reminder tool instructions, skill guidance, README + CLAUDE.md patterns"
```

---

### Task 14: Extension — generalize to `alexandria/` with feature toggles

**Files:**

- Rename: `contrib/pi/extensions/alexandria-auto-recall/` → `contrib/pi/extensions/alexandria/` (`git mv`)
- Modify: `contrib/pi/extensions/alexandria/package.json`, `src/mcp-client.ts` (client name), `src/config.ts`, `src/index.ts`
- Modify: `contrib/pi/README.md`, extension `README.md`

**Step 1: Rename**

```bash
git mv contrib/pi/extensions/alexandria-auto-recall contrib/pi/extensions/alexandria
```

In `package.json`: `"name": "alexandria"`, `"version": "2.1.0"`. In `src/mcp-client.ts`: client name `"alexandria-auto-recall"` → `"alexandria"`.

**Step 2: config.ts** — extend `ClientToml` and `CONFIG`:

```ts
interface ClientToml {
 server?: { url?: string };
 recall?: { enabled?: boolean; limit?: number; min_similarity?: number };
 store?: {
  enabled?: boolean;
  extract_model?: string;
  extract_timeout_ms?: number;
 };
 reminders?: { enabled?: boolean; project?: string };
}
```

Add to `CONFIG` (mirrors the existing `storeDisabled` precedence shape — env wins, TOML `enabled=false` honored only when env is unset):

```ts
 remindersDisabled:
  process.env.ALEXANDRIA_REMINDERS === "off" ||
  (toml.reminders?.enabled === false &&
   process.env.ALEXANDRIA_REMINDERS === undefined),

 remindersProject:
  process.env.ALEXANDRIA_REMINDERS_PROJECT ?? toml.reminders?.project,
```

> **Design-doc deviation (intentional):** the design doc sketched `[features.recall]`-style nesting; the existing client.toml already uses flat `[recall]`/`[store]` sections with `enabled` keys, so `[reminders]` follows suit. Same user-visible capability, zero churn. Legacy `ALEXANDRIA_AUTO_RECALL=off` alias already works — untouched.

**Step 3: Create `src/reminders.ts`:**

```ts
/**
 * Reminders feature: check Alexandria for due reminders on each prompt.
 * Fail-open everywhere — an unreachable server never blocks the turn.
 */

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { basename } from "node:path";
import { callToolWithRetry, extractTextContent } from "./mcp-client.js";
import { CONFIG } from "./config.js";

const execFileAsync = promisify(execFile);

export interface DueReminder {
 id: string;
 message: string;
 target?: string;
 escalated?: boolean;
 recurring?: boolean;
 missed_occurrences?: number;
 due_at?: string;
 schedule?: string;
 note?: string | null;
 provenance?: { project?: string | null; session_id?: string | null };
}

/** Project hint for delivery targeting. Env override wins (direnv-friendly,
 *  also fixes git-worktree dirs whose basename differs from the project). */
export async function getProjectHint(): Promise<string | undefined> {
 if (CONFIG.remindersProject) return CONFIG.remindersProject;
 try {
  const { stdout } = await execFileAsync(
   "git",
   ["rev-parse", "--show-toplevel"],
   { timeout: 2000 },
  );
  const root = stdout.trim();
  return root ? basename(root) : undefined;
 } catch {
  return undefined; // not a repo, git missing — global reminders still work
 }
}

export async function checkReminders(
 project?: string,
): Promise<DueReminder[]> {
 const args: Record<string, unknown> = project ? { project } : {};
 const result = await callToolWithRetry("check_reminders", args);
 const text = extractTextContent(result.content);
 if (!text) return [];
 try {
  const parsed = JSON.parse(text) as { delivered?: DueReminder[] };
  return parsed.delivered ?? [];
 } catch {
  return [];
 }
}

export function formatDueBlock(items: DueReminder[]): string {
 const lines = items.map((r) => {
  const bits: string[] = [`- ${r.message}`];
  if (r.target && r.target !== "global") bits.push(`[target ${r.target}]`);
  if (r.escalated) bits.push("[OVERDUE — escalated from project targeting]");
  if (r.missed_occurrences && r.missed_occurrences > 0)
   bits.push(`(missed ${r.missed_occurrences} earlier occurrence(s))`);
  if (r.schedule) bits.push(`(${r.schedule})`);
  if (r.note) bits.push(`note: ${r.note}`);
  const prov = r.provenance;
  if (prov?.project || prov?.session_id)
   bits.push(`[set in ${prov.project ?? "unknown project"}${prov.session_id ? `, session ${prov.session_id}` : ""}]`);
  return bits.join(" ");
 });
 return [
  "⏰ Due reminders from Alexandria (delivered once — act on them or tell the user, then they're gone):",
  ...lines,
 ].join("\n");
}
```

**Step 4: Rewrite the `before_agent_start` recall handler in `index.ts` as a single dispatcher** that runs recall + reminders concurrently with per-feature failure isolation. (One merged handler keeps the ordering deterministic and injects exactly one message per prompt — pi does collect a `message` from each `before_agent_start` handler, so this is a choice, not a workaround.) Replace the existing `if (!CONFIG.recallDisabled) { pi.on("before_agent_start", ...) }` block with:

```ts
 // ── Combined injection dispatcher (recall + reminders) ──────────────
 if (!CONFIG.recallDisabled || !CONFIG.remindersDisabled) {
  pi.on("before_agent_start", async (event, ctx) => {
   const query = event.prompt?.trim();

   const recallTask: Promise<string | null> =
    !CONFIG.recallDisabled && query
     ? (async () => {
       const memories = await retrieveMemories(query);
       return memories.length > 0 ? formatMemoriesBlock(memories) : null;
      })()
     : Promise.resolve(null);

   const remindersTask: Promise<{ block: string | null; count: number }> =
    !CONFIG.remindersDisabled
     ? (async () => {
       const project = await getProjectHint();
       const due = await checkReminders(project);
       return {
        block: due.length > 0 ? formatDueBlock(due) : null,
        count: due.length,
       };
      })()
     : Promise.resolve({ block: null, count: 0 });

   // Per-feature failure isolation: one failing never suppresses the other
   const [recallRes, remindersRes] = await Promise.allSettled([
    recallTask,
    remindersTask,
   ]);

   const blocks: string[] = [];
   if (recallRes.status === "fulfilled" && recallRes.value) {
    blocks.push(recallRes.value);
   } else if (recallRes.status === "rejected") {
    resetClient();
    ctx.ui.notify(
     `Alexandria auto-recall failed (${recallRes.reason instanceof Error ? recallRes.reason.message : String(recallRes.reason)}); continuing without it.`,
     "warning",
    );
   }
   if (remindersRes.status === "fulfilled") {
    if (remindersRes.value.block) {
     blocks.push(remindersRes.value.block);
     ctx.ui.notify(
      `⏰ ${remindersRes.value.count} Alexandria reminder(s) due`,
      "info",
     );
    }
   } else {
    ctx.ui.notify(
     `Alexandria reminders check failed (${remindersRes.reason instanceof Error ? remindersRes.reason.message : String(remindersRes.reason)}); continuing without it.`,
     "warning",
    );
   }

   if (blocks.length === 0) return;
   return {
    message: {
     customType: "alexandria",
     content: blocks.join("\n\n"),
     display: true,
    },
   };
  });
 }
```

Imports to add at top of `index.ts`:

```ts
import { getProjectHint, checkReminders, formatDueBlock } from "./reminders.js";
```

Update the file's header comment block: rename to "Alexandria Companion Extension (v2.1)", document the three features (recall / store / reminders) and the new env vars (`ALEXANDRIA_REMINDERS=off`, `ALEXANDRIA_REMINDERS_PROJECT`).

> **Note**: injected-message `customType` changes from `"alexandria-auto-recall"` to `"alexandria"`. Search the repo (`rg alexandria-auto-recall`) for any other references (tests, docs) and update them.

**Step 5: READMEs.** Update `contrib/pi/extensions/alexandria/README.md`: new dir name, install path, features table (recall/store/reminders + toggles), reminders behavior (per-prompt check, targeting, escalation, worktree note, `ALEXANDRIA_REMINDERS_PROJECT`). Update `contrib/pi/README.md` dir reference.

**Step 6: Verify** — `cd contrib/pi/extensions/alexandria && npm install && npx tsc --noEmit` → clean.

**Step 7: Commit**

```bash
git add -A contrib/pi/
git commit -m "feat(extension): generalize to alexandria companion with reminders feature"
```

---

### Task 15: Extension smoke tests + final verification

**Files:**

- Create: `contrib/pi/extensions/alexandria/tests/reminders.test.ts`
- Create: `contrib/pi/extensions/alexandria/tests/config.test.ts`
- Modify: `contrib/pi/extensions/alexandria/package.json` (test script + tsx devDep)

**Step 1: Add test infra.** In `package.json`:

```json
  "scripts": { "test": "tsx --test tests/" },
  "devDependencies": { "tsx": "^4.19.0", ... }
```

(`npm install` picks up tsx. Node ≥ 20 provides `node:test`.)

**Step 2: Write tests.**

`tests/reminders.test.ts` — pure functions only (no network):

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { formatDueBlock, type DueReminder } from "../src/reminders.js";

test("formatDueBlock renders message, target, escalation, missed count", () => {
 const block = formatDueBlock([
  { id: "reminder:1", message: "renew cert", target: "project:infra", escalated: true, missed_occurrences: 2 },
  { id: "reminder:2", message: "standup", schedule: "every Friday at 09:00" },
 ]);
 assert.match(block, /⏰ Due reminders/);
 assert.match(block, /renew cert/);
 assert.match(block, /OVERDUE/);
 assert.match(block, /missed 2 earlier/);
 assert.match(block, /every Friday at 09:00/);
});

test("formatDueBlock empty list still renders header", () => {
 assert.match(formatDueBlock([]), /Due reminders/);
});
```

`tests/config.test.ts` — env alias resolution (set env before importing CONFIG via dynamic import; CONFIG reads env at module load):

```ts
import { test } from "node:test";
import assert from "node:assert/strict";

test("legacy ALEXANDRIA_AUTO_RECALL=off disables recall", async () => {
 process.env.ALEXANDRIA_AUTO_RECALL = "off";
 const { CONFIG } = await import("../src/config.js?" + Date.now()); // bust module cache
 assert.equal(CONFIG.recallDisabled, true);
 delete process.env.ALEXANDRIA_AUTO_RECALL;
});

test("ALEXANDRIA_REMINDERS=off disables reminders", async () => {
 process.env.ALEXANDRIA_REMINDERS = "off";
 const { CONFIG } = await import("../src/config.js?" + Date.now());
 assert.equal(CONFIG.remindersDisabled, true);
 delete process.env.ALEXANDRIA_REMINDERS;
});

test("reminders enabled by default", async () => {
 const { CONFIG } = await import("../src/config.js?" + Date.now());
 assert.equal(CONFIG.remindersDisabled, false);
});
```

> If cache-busting via query string doesn't work under tsx, restructure config.ts to export a `loadConfig()` factory the tests can call after mutating env (keep `CONFIG` as the singleton built from it — no behavior change).

**Step 3: Run** — `cd contrib/pi/extensions/alexandria && npm install && npm test` — expect PASS.

**Step 4: Full workspace verification.**

```bash
cd ~/code/.worktrees/alexandria/feature/reminders
just ci                                  # fmt + lint (-Dwarnings) + test + deny
cargo test -p alexandria --test reminders_test -- --ignored   # the ~70s recurring test
```

Expected: all green. `cargo deny` may flag the new deps' licenses — cron (MIT/Apache-2.0), chrono-tz (MIT), iana-time-zone (MIT/Apache-2.0) are all permissive; if deny.toml needs updating, note it in the commit.

**Step 5: Commit**

```bash
git add contrib/pi/extensions/alexandria/ Cargo.lock
git commit -m "test(extension): reminders formatting + config toggle tests; final ci green"
```

---

## Manual end-to-end verification (after all tasks)

1. `just run` with `[server] transport = "http"` (or your usual config)
2. In a pi session with the renamed extension installed (`cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/ && cd ~/.pi/agent/extensions/alexandria && npm install`):
   - "remind me in 2 minutes to test the reminder flow" → verify `next_fire_preview` confirmation
   - Send any prompt ~2.5 min later → verify `⏰` block in context + notify, and that a second prompt does NOT re-deliver
   - "remind me every Friday at 9am to prep standup, for this project" → verify weekly pattern + project targeting; open pi in a different repo → verify it does NOT deliver (before escalation)
3. `alexandria_list_reminders` → verify rendering; cancel one → verify gone from pending

## Deviations from the design doc (recorded)

- **Client config shape**: flat `[reminders] enabled/project` in client.toml instead of `[features.*]` nesting — matches the existing `[recall]`/`[store]` convention (same capability).
- **No `Feature` interface abstraction**: features are config-guarded modules dispatched from one handler; a formal interface would be premature abstraction for three features (AGENTS.md minimal-abstraction rule). The dispatcher still delivers per-feature failure isolation as designed.
- **Single merged injection handler**: recall + reminders blocks are merged into one `before_agent_start` injection (customType `alexandria`) for deterministic ordering and exactly one injected message per prompt. pi does merge messages from several handlers, so this is a design choice rather than a limitation.
- **`status` gained a third value**: the design specified `pending | cancelled` with "no separate delivered state"; the implementation stores `status = 'delivered'` for consumed one-shots (v007 schema `ASSERT`, the `Reminder` model comment, and the `list_reminders` filter). Defensible because a NULL `next_due_at` is still matched by `next_due_at <= $now` in SurrealQL, so `'delivered'` is the only honest terminal marker — but it is both a schema change and a user-facing surface change.
- **Preview-first `next_fire_preview`**: `set_reminder` returns a `next_fire_preview` of 3 fire times whose first element *is* `next_due_at`, where the design asked for "next_due_at plus the next 3 fire times". Same instants, one fewer duplicate, and the field the caller confirms against is the first thing they see.
- **Flat `project` param**: `check_reminders` takes a flat `project` where the design specified `context: { project }` — matches the flat params of the other reminder tools and avoids a one-key wrapper object.
- **Inclusive escalation boundary**: a project-targeted reminder escalates at `>= escalation_hours` overdue where the design text said "longer than". The inclusive form is what makes `escalation_hours = 0` mean "escalate as soon as overdue" instead of never.
- **Claude Code client has no automatic delivery** (2026-09-14 merge): `contrib/claude/hooks/alexandria-recall.sh` injects auto-recall only, so a Claude Code user sees the read-only `due_reminders` piggyback on `retrieve_memories`/`recall` but gets no `check_reminders` consumption unless the agent calls the tool itself. Implementing delivery in the hook was left out of scope; the Pi companion extension is the shipped delivery path.
- **Migration renumbered v006 → v007** (2026-09-14 merge): main claimed version 6 for `drop_session_memory_count` while this branch was open, so the reminder table shipped as `schema/v007_reminder.surql`. `migrate()` only applies `version > current_version`, so leaving it at 6 would have made every database already migrated past main's v6 silently skip the reminder schema. Steps 4/6/12 below still say v006 — that is what was written and executed at the time.
