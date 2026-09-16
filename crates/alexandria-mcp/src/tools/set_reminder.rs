#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ReminderPatternParams {
    #[schemars(description = "Recurrence frequency: 'daily', 'weekly', or 'monthly'")]
    pub freq: String,
    #[schemars(
        description = "Wall-clock time in the server's configured timezone, HH:MM (24h), e.g. '09:00'"
    )]
    pub time: String,
    #[schemars(
        description = "For freq=weekly: list of weekday names, e.g. ['mon','fri']. Omit for daily/monthly."
    )]
    pub weekdays: Option<Vec<String>>,
    #[schemars(
        description = "For freq=monthly: day of month 1-31. Months without that day are skipped. Omit for daily/weekly."
    )]
    pub day_of_month: Option<u32>,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct SetReminderParams {
    #[schemars(
        description = "The reminder text to deliver; must not be blank. Write it standalone — it will be shown in a future session without today's conversation."
    )]
    pub message: String,
    #[schemars(
        description = "One-shot: ISO-8601 datetime. Explicit offsets honored; naive timestamps are interpreted in the server timezone, and a local time that a DST transition removed (spring-forward gap) or duplicated (fall-back fold) is rejected — name an offset or move the minute. Exactly one of due_at/pattern/cron must be given."
    )]
    pub due_at: Option<String>,
    #[schemars(
        description = "Recurring named pattern: {freq: daily|weekly|monthly, time: 'HH:MM', weekdays?, day_of_month?}. Times are wall clock in the server timezone: a daily 09:00 stays 09:00 local across DST, a time skipped by a spring-forward gap does not fire that day, and a time inside a fall-back fold fires once (on the first pass). A monthly day_of_month is skipped in short months. Exactly one of due_at/pattern/cron must be given."
    )]
    pub pattern: Option<ReminderPatternParams>,
    #[schemars(
        description = "Recurring cron escape hatch: 5, 6 or 7 fields, evaluated in the server timezone (a 5-field 'min hour dom mon dow' expression gets seconds prepended). Two dialect traps, both silent: (1) days of week are NOT standard cron — numeric dow runs 1=Sunday..7=Saturday and 0 is rejected, so '0 9 * * MON-FRI' is weekdays at 09:00 while '0 9 * * 1-5' is Sunday–Thursday; write day names. (2) day-of-month and day-of-week are ANDed, not ORed as in Vixie cron, when both are restricted — '0 9 13 * FRI' fires only on Friday the 13th, so a union takes two reminders. @-shorthands are rejected; write them out. Exactly one of due_at/pattern/cron must be given. Prefer pattern unless you need cron-only expressiveness."
    )]
    pub cron: Option<String>,
    #[schemars(
        description = "Optional project name to target delivery (e.g. 'alexandria'). Omit for a global reminder. Project reminders escalate to global delivery once they are at least escalation_hours overdue, so they are never silently lost. Matching is exact and case-sensitive — the basename of a worktree directory is a different project name."
    )]
    pub target_project: Option<String>,
    #[schemars(
        description = "Optional provenance: project this reminder was set in (display metadata only, does not affect delivery)"
    )]
    pub prov_project: Option<String>,
    #[schemars(
        description = "Optional provenance: session ID this reminder was set in (display metadata only)"
    )]
    pub session_id: Option<String>,
    #[schemars(
        description = "Optional extra context shown alongside the reminder at delivery time"
    )]
    pub note: Option<String>,
}
