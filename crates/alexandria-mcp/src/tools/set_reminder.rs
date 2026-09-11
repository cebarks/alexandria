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
        description = "The reminder text to deliver. Write it standalone — it will be shown in a future session without today's conversation."
    )]
    pub message: String,
    #[schemars(
        description = "One-shot: ISO-8601 datetime. Explicit offsets honored; naive timestamps are interpreted in the server timezone. Exactly one of due_at/pattern/cron must be given."
    )]
    pub due_at: Option<String>,
    #[schemars(
        description = "Recurring named pattern: {freq: daily|weekly|monthly, time: 'HH:MM', weekdays?, day_of_month?}. Exactly one of due_at/pattern/cron must be given."
    )]
    pub pattern: Option<ReminderPatternParams>,
    #[schemars(
        description = "Recurring cron escape hatch: standard 5-field cron (min hour dom mon dow), evaluated in the server timezone. Exactly one of due_at/pattern/cron must be given. Prefer pattern unless you need cron-only expressiveness."
    )]
    pub cron: Option<String>,
    #[schemars(
        description = "Optional project name to target delivery (e.g. 'alexandria'). Omit for a global reminder. Project reminders escalate to global delivery if overdue too long, so they are never silently lost."
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
