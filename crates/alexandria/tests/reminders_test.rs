use std::sync::Arc;

use alexandria_engine::reminders::{human_readable, spec_from_reminder};
use alexandria_mcp::server::{AlexandriaServer, RemindersSettings};
use alexandria_mcp::tools::{
    CancelReminderParams, CheckRemindersParams, ListRemindersParams, ReminderPatternParams,
    SetReminderParams,
};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::models::schedule_kind;
use alexandria_storage::repos::{NewReminder, ReminderRepo};
use alexandria_storage::{Database, schema};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Timelike, Utc};

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
    setup_with_tz(tz()).await
}

/// Non-UTC variant: naive datetimes are interpreted in the configured timezone
/// and responses carry a local rendering alongside the UTC one.
async fn setup_with_tz(tz: chrono_tz::Tz) -> AlexandriaServer {
    setup_with_escalation(tz, 48).await
}

/// Server with an explicit escalation window. Delivery tests each name their
/// own `escalation_hours` because that value is the behaviour under test.
async fn setup_with_escalation(tz: chrono_tz::Tz, escalation_hours: u64) -> AlexandriaServer {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();
    AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0)
        .with_reminders_config(RemindersSettings {
            tz,
            escalation_hours,
        })
}

/// A rejected schedule must not leave a row behind: a stored row is a promise of
/// delivery, and `list_due` can never pick up one written with NULL next_due_at.
async fn assert_no_rows(server: &AlexandriaServer) {
    let rows = ReminderRepo::new(server.db.inner())
        .list(None, None, 100, 0)
        .await
        .unwrap()
        .0;
    assert!(
        rows.is_empty(),
        "rejected schedule still wrote {} row(s): {:?}",
        rows.len(),
        rows.iter().map(|r| r.message.clone()).collect::<Vec<_>>()
    );
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

fn weekly_params(msg: &str) -> SetReminderParams {
    SetReminderParams {
        message: msg.to_string(),
        due_at: None,
        pattern: Some(ReminderPatternParams {
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
    }
}

fn cron_params(msg: &str, expr: &str) -> SetReminderParams {
    SetReminderParams {
        message: msg.to_string(),
        due_at: None,
        pattern: None,
        cron: Some(expr.to_string()),
        target_project: None,
        prov_project: None,
        session_id: None,
        note: None,
    }
}

/// A one-shot that came due `duration_secs` ago.
///
/// Due-ness and escalation are both computed from `now - next_due_at`, so
/// back-dating at set time reproduces "has been waiting a while" without the
/// test having to sleep for it — which matters when the wait is an hour.
fn once_params_overdue(msg: &str, duration_secs: i64) -> SetReminderParams {
    let due = Utc::now() - chrono::Duration::seconds(duration_secs);
    once_params(msg, &due.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn monthly_params(msg: &str, day_of_month: Option<u32>) -> SetReminderParams {
    SetReminderParams {
        message: msg.to_string(),
        due_at: None,
        pattern: Some(ReminderPatternParams {
            freq: "monthly".to_string(),
            time: "09:00".to_string(),
            weekdays: None,
            day_of_month,
        }),
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
    let p = weekly_params("standup");
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

/// Round-trip guard: every schedule kind the writer accepts must be readable
/// back by `spec_from_reminder` and render the same `schedule` string the
/// response promised. This locks the writer-side row contract (schedule_kind
/// discriminators, per-freq `day_of_month`, normalized cron) to the reader side.
#[tokio::test]
async fn set_reminder_rows_round_trip_through_spec_from_reminder() {
    let server = setup().await;
    let cases = [
        (
            "once",
            once_params("round trip once", "2030-01-01T12:00:00Z"),
            None,
        ),
        ("pattern", weekly_params("round trip weekly"), None),
        (
            "pattern",
            monthly_params("round trip monthly", Some(15)),
            Some(15),
        ),
        ("cron", cron_params("round trip cron", "0 9 * * *"), None),
        // A padded 5-field expression is the shape that used to break the
        // writer/reader equality: `normalize_cron` prepended seconds without
        // collapsing the padding, so the row stored a 6-field string *with a
        // trailing space* that the reader then trimmed — one row, two different
        // `schedule` renderings, and `cron '...'` is exactly the string an agent is
        // told to confirm against.
        (
            "cron",
            cron_params("round trip padded cron", "  0 9 * * 1-5  "),
            None,
        ),
    ];

    for (expected_kind, params, expected_dom) in cases {
        let result = server
            .do_set_reminder(params)
            .await
            .unwrap_or_else(|e| panic!("set_reminder ({expected_kind}) failed: {e:#}"));
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        let id = v["id"].as_str().unwrap().to_string();

        let row = ReminderRepo::new(server.db.inner())
            .get(&id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("row {id} not found"));

        assert_eq!(row.schedule_kind, expected_kind, "kind mismatch for {id}");
        // day_of_month is stored only for monthly patterns; every other kind
        // stores NULL (spec_from_reminder rejects junk but defaults NULL).
        assert_eq!(
            row.day_of_month, expected_dom,
            "day_of_month mismatch for {id}"
        );
        if expected_dom == Some(15) {
            assert_eq!(
                v["schedule"], "monthly on day 15 at 09:00",
                "monthly rendering drifted for {id}"
            );
        }

        let spec = spec_from_reminder(&row)
            .unwrap_or_else(|e| panic!("spec_from_reminder failed for {id}: {e:#}"));
        assert_eq!(
            human_readable(&spec),
            v["schedule"].as_str().unwrap(),
            "schedule rendering drifted between writer and reader for {id}"
        );
    }

    // Constants are the single spelling of the discriminator set.
    assert_eq!(schedule_kind::ONCE, "once");
    assert_eq!(schedule_kind::PATTERN, "pattern");
    assert_eq!(schedule_kind::CRON, "cron");
}

/// A recurring schedule that never fires again would be stored with
/// next_due_at = NULL, which `list_due` (`next_due_at <= $now`) can never
/// select — undeliverable and unescalatable. It must be rejected, not stored.
#[tokio::test]
async fn set_reminder_rejects_never_firing_cron() {
    let server = setup().await;
    // 7-field cron pinned to a year already past, and an impossible date.
    for expr in ["0 0 12 * * * 2020", "0 0 0 30 2 *"] {
        let err = server
            .do_set_reminder(cron_params("never fires", expr))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("never fires again"),
            "unexpected error for {expr}: {err}"
        );
        assert!(
            err.contains(expr),
            "error must name the offending expression: {err}"
        );
    }
    assert_no_rows(&server).await;
}

/// Cross-field day_of_month rules must be reachable from the writer: the engine
/// already refuses to guess "the 1st" on the reader side, so a monthly pattern
/// with no day must not be silently stored as day 1.
#[tokio::test]
async fn set_reminder_rejects_monthly_without_day_of_month() {
    let server = setup().await;
    let err = server
        .do_set_reminder(monthly_params("pay rent", None))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("day_of_month"),
        "error must name the missing field: {err}"
    );
    assert_no_rows(&server).await;
}

/// Same rule from the other side: day_of_month is monthly-only, so weekly and
/// daily must reject it rather than drop it silently.
#[tokio::test]
async fn set_reminder_rejects_day_of_month_on_weekly_and_daily() {
    let server = setup().await;
    for freq in ["weekly", "daily"] {
        let mut p = weekly_params("standup");
        let pat = p.pattern.as_mut().unwrap();
        pat.freq = freq.to_string();
        pat.day_of_month = Some(15);
        if freq == "daily" {
            // Daily rejects weekdays too; drop them so the dom failure is the
            // one under test.
            pat.weekdays = None;
        }
        let err = server.do_set_reminder(p).await.unwrap_err().to_string();
        assert!(
            err.contains("day_of_month"),
            "error must name the offending field for freq={freq}: {err}"
        );
    }
    assert_no_rows(&server).await;
}

/// Every parse failure must be identifiable from the message alone: name the
/// field and echo the offending input.
#[tokio::test]
async fn set_reminder_bad_time_error_names_field_and_input() {
    let server = setup().await;
    for (input, field) in [
        ("9:00 AM", "minute"),
        ("ab:00", "hour"),
        ("09:ab", "minute"),
    ] {
        let mut p = weekly_params("standup");
        p.pattern.as_mut().unwrap().time = input.to_string();
        let err = server.do_set_reminder(p).await.unwrap_err().to_string();
        assert!(err.contains("time"), "error must name the field: {err}");
        assert!(err.contains(field), "error must name the sub-field: {err}");
        assert!(
            err.contains(input),
            "error must echo the offending input: {err}"
        );
    }
    assert_no_rows(&server).await;
}

/// The tool response flattens the whole error chain (`{e:#}`), so the cron
/// crate's real reason must survive instead of a bare "invalid cron expression".
/// (Whitespace collapsing happens in the MCP wrapper; see
/// `server::error_message_tests::flattens_chain_and_collapses_whitespace`.)
#[tokio::test]
async fn set_reminder_bad_cron_error_carries_underlying_reason() {
    let server = setup().await;
    let err = server
        .do_set_reminder(cron_params("x", "61 99 * * *"))
        .await
        .unwrap_err();
    let flat = format!("{err:#}");
    assert!(
        flat.contains("61 99 * * *"),
        "error must name the expression: {flat}"
    );
    assert!(
        flat.contains("Minutes must be less than 59"),
        "error must carry the cron crate's reason: {flat}"
    );
    assert_no_rows(&server).await;
}

/// The multiple-schedules failure must say which fields collided.
#[tokio::test]
async fn set_reminder_exactly_one_error_names_given_fields() {
    let server = setup().await;

    let mut both = once_params("x", "2030-01-01T12:00:00Z");
    both.cron = Some("0 9 * * *".to_string());
    let err = server.do_set_reminder(both).await.unwrap_err().to_string();
    assert!(
        err.contains("due_at + cron"),
        "error must name the colliding fields: {err}"
    );

    let mut none = once_params("x", "2030-01-01T12:00:00Z");
    none.due_at = None;
    let err = server.do_set_reminder(none).await.unwrap_err().to_string();
    assert!(err.contains("got none"), "unexpected error: {err}");

    assert_no_rows(&server).await;
}

/// A blank `target_project` is not "no target": `None` is global, while `"  "`
/// matches no project hint (delivery matching is byte-exact), so the row could
/// only ever be held and then escalated — an empty string is a reminder with
/// nowhere to arrive, written as if it were targeted.
#[tokio::test]
async fn set_reminder_rejects_blank_target_project() {
    let server = setup().await;
    for blank in ["", "   ", "\t", " \n "] {
        let mut p = once_params("x", "2030-01-01T12:00:00Z");
        p.target_project = Some(blank.to_string());
        let err = server.do_set_reminder(p).await.unwrap_err().to_string();
        assert!(
            err.contains("target_project"),
            "error must name the field for {blank:?}: {err}"
        );
        assert!(
            err.contains("empty"),
            "error must say what is wrong for {blank:?}: {err}"
        );
        assert_no_rows(&server).await;
    }

    // A real name is untouched by the check, and is stored verbatim (byte-exact
    // matching means a padded value is the caller's business, not a blank one).
    let mut p = once_params("fine", "2030-01-01T12:00:00Z");
    p.target_project = Some("infra".to_string());
    let v: serde_json::Value =
        serde_json::from_str(&server.do_set_reminder(p).await.unwrap()).unwrap();
    assert_eq!(v["status"], "ok");
    let row = ReminderRepo::new(server.db.inner())
        .get(v["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.target_project.as_deref(), Some("infra"));
}

/// Duplicate/misspelled weekdays must not leak into the user-facing schedule
/// string or into the stored row.
#[tokio::test]
async fn set_reminder_dedupes_and_normalizes_weekdays() {
    let server = setup().await;
    let mut p = weekly_params("standup");
    p.pattern.as_mut().unwrap().weekdays = Some(vec![
        "fri".to_string(),
        "FRI".to_string(),
        "friday".to_string(),
    ]);
    let result = server.do_set_reminder(p).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["schedule"], "every Friday at 09:00");

    let id = v["id"].as_str().unwrap().to_string();
    let row = ReminderRepo::new(server.db.inner())
        .get(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.weekdays, vec!["fri".to_string()]);
    let spec = spec_from_reminder(&row).unwrap();
    assert_eq!(human_readable(&spec), "every Friday at 09:00");
}

/// Response datetimes keep a constant second-precision shape even when the
/// input carries sub-second digits (reminder granularity is the minute).
#[tokio::test]
async fn set_reminder_next_due_at_keeps_second_precision() {
    let server = setup().await;
    let result = server
        .do_set_reminder(once_params("fractional", "2030-01-01T12:00:00.500Z"))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["next_due_at"], "2030-01-01T12:00:00Z");
    assert_eq!(v["next_fire_preview"][0], "2030-01-01T12:00:00Z");
}

/// In a non-UTC timezone the response carries both the UTC instant and the
/// local wall-clock spelling the user actually asked for.
#[tokio::test]
async fn set_reminder_non_utc_tz_reports_local_next_due() {
    let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
    let server = setup_with_tz(ny).await;

    // Naive input is interpreted in the configured timezone.
    let result = server
        .do_set_reminder(once_params("review PRs", "2030-01-01T15:00"))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["timezone"], "America/New_York");
    assert_eq!(v["next_due_at"], "2030-01-01T20:00:00Z");
    assert_eq!(v["next_due_at_local"], "2030-01-01 15:00 EST");

    // A pattern's local rendering carries the schedule's wall-clock time; the
    // abbreviation is whatever America/New_York is in on the run date.
    let result = server
        .do_set_reminder(weekly_params("standup"))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["schedule"], "every Friday at 09:00");
    let local = v["next_due_at_local"].as_str().unwrap().to_string();
    let (local_dt, abbr) = local.rsplit_once(' ').unwrap();
    assert!(
        local_dt.ends_with("09:00"),
        "local rendering must keep the wall-clock time: {local}"
    );
    assert!(
        abbr == "EST" || abbr == "EDT",
        "unexpected timezone abbreviation: {local}"
    );
}

// --- check_reminders: delivery, targeting, escalation, consumption ---

fn check(project: Option<&str>) -> CheckRemindersParams {
    CheckRemindersParams {
        project: project.map(str::to_string),
    }
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
    // The shape Task 14's pi companion parses: a global one-shot is never
    // escalated, never has missed occurrences, and is exhausted by delivery.
    assert_eq!(out["delivered"][0]["target"], "global");
    assert_eq!(out["delivered"][0]["escalated"], false);
    assert_eq!(out["delivered"][0]["recurring"], false);
    assert_eq!(out["delivered"][0]["missed_occurrences"], 0);
    assert_eq!(out["delivered"][0]["due_at"], "2020-01-01T12:00:00Z");
    assert!(out["delivered"][0]["next_due_at"].is_null());

    // second check: consumed, nothing due
    let out2: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out2["count"], 0);
}

/// Project targeting inside the hold window: a project reminder nobody in the
/// matching context has seen is *held*, not escalated — `setup()` runs with the
/// default 48 h window and this one is only ~1 minute overdue.
#[tokio::test]
async fn check_project_targeting_and_escalation() {
    let server = setup().await;
    let mut p = once_params_overdue("renew cert", 75);
    p.target_project = Some("infra".to_string());
    server.do_set_reminder(p).await.unwrap();

    // wrong project → not delivered
    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_check_reminders(check(Some("other")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out["count"], 0);

    // right project → delivered, and delivered as a match rather than an
    // escalation: the hold must not be silently rewritten into "escalated".
    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_check_reminders(check(Some("infra")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["escalated"], false);
}

/// Zero window: every overdue project reminder escalates immediately.
#[tokio::test]
async fn check_escalates_long_overdue_project_reminder() {
    // escalation_hours = 0 in this server → any overdue project reminder escalates
    let server = setup_with_escalation(tz(), 0).await;

    let mut p = once_params("stale project thing", "2020-01-01T12:00:00Z");
    p.target_project = Some("abandoned".to_string());
    server.do_set_reminder(p).await.unwrap();

    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_check_reminders(check(Some("elsewhere")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["escalated"], true);
    assert_eq!(out["delivered"][0]["target"], "project:abandoned");
}

/// A row written straight through the repo, bypassing `do_set_reminder`.
///
/// The writer validates every schedule and refuses a NULL `next_due_at`, so the
/// delivery loop's per-row failure paths can only be reached by a row that got
/// in some other way — a direct edit, or a migration an operator runs. Writing
/// it here is how those paths get tested without a raw query; the caller
/// overwrites whichever fields make the row unreadable.
fn raw_row(message: &str, next_due_at: Option<DateTime<Utc>>) -> NewReminder {
    NewReminder {
        message: message.to_string(),
        target_project: None,
        prov_project: None,
        prov_session_id: None,
        note: None,
        schedule_kind: schedule_kind::ONCE.to_string(),
        due_at: None, // an unreadable `once` row; readable rows set what they need
        freq: None,
        time_of_day: None,
        weekdays: vec![],
        day_of_month: None,
        cron_expr: None,
        next_due_at,
    }
}

/// An `escalation_hours` value the server cannot turn into a duration must not
/// take the check down with it: chrono's `Duration::hours` panics ("TimeDelta::
/// hours out of bounds") well below `u64::MAX`, at i64::MAX/3600 hours, so
/// saturating the `u64`→`i64` step is not enough — and a panic here would kill
/// the stdio serve loop or the HTTP request task on *every* interaction. Such a
/// window is unmeasurably long, so the row is held rather than escalated.
#[tokio::test]
async fn check_unrepresentable_escalation_window_holds_instead_of_panicking() {
    // i64::MAX (the old saturation target) is itself past the bound; the second
    // value is the smallest absurd one used here, past i64::MAX/3600.
    for hours in [u64::MAX, 2_600_000_000_000_000] {
        let server = setup_with_escalation(tz(), hours).await;
        let mut p = once_params("ancient project thing", "2020-01-01T12:00:00Z");
        p.target_project = Some("abandoned".to_string());
        server.do_set_reminder(p).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(
            &server
                .do_check_reminders(check(Some("elsewhere")))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["count"], 0, "absurd window {hours} must not escalate");

        // Held, not lost: it still reaches its own project as a plain match.
        let out: serde_json::Value = serde_json::from_str(
            &server
                .do_check_reminders(check(Some("abandoned")))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["delivered"][0]["escalated"], false);
    }
}

/// Handoff note 4's isolation guarantee at the delivery layer: a row the loop
/// cannot handle is skipped and left pending, and never costs the healthy rows
/// their delivery. Engine tests cover `spec_from_reminder` itself; only the loop
/// shows that a skip is per-row rather than a failed check.
///
/// `escalation_hours: 0` is deliberate — the NULL-`next_due_at` row is
/// project-targeted, and measuring its age as `now - now` would escalate and
/// deliver it here (with `due_at: null`) instead of skipping it.
#[tokio::test]
async fn check_skips_corrupt_rows_and_still_delivers_the_healthy_one() {
    let server = setup_with_escalation(tz(), 0).await;
    let repo = ReminderRepo::new(server.db.inner());
    let past = Utc::now() - chrono::Duration::minutes(5);

    // A readable global one-shot that came due five minutes ago.
    let healthy = repo
        .create(&NewReminder {
            due_at: Some(past),
            ..raw_row("healthy row", Some(past))
        })
        .await
        .unwrap();

    // Every shape the delivery loop has to survive on a row `list_due` selects.
    let mut without_freq = raw_row("pattern without freq", Some(past));
    without_freq.schedule_kind = schedule_kind::PATTERN.to_string();
    without_freq.time_of_day = Some("09:00".to_string());

    let mut junk_weekday = raw_row("pattern with junk weekday", Some(past));
    junk_weekday.schedule_kind = schedule_kind::PATTERN.to_string();
    junk_weekday.freq = Some("weekly".to_string());
    junk_weekday.time_of_day = Some("09:00".to_string());
    junk_weekday.weekdays = vec!["Fribble".to_string()];

    let mut impossible_cron = raw_row("cron out of range", Some(past));
    impossible_cron.schedule_kind = schedule_kind::CRON.to_string();
    impossible_cron.cron_expr = Some("99 99 99 99 99".to_string());

    let mut no_next_due = raw_row("NULL next_due_at", None);
    no_next_due.due_at = Some(past);
    no_next_due.target_project = Some("ghost".to_string());

    let corrupt = [
        raw_row("once without due_at", Some(past)),
        without_freq,
        junk_weekday,
        impossible_cron,
        no_next_due,
    ];
    let mut corrupt_ids = Vec::new();
    for row in &corrupt {
        corrupt_ids.push(repo.create(row).await.unwrap());
    }

    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out["count"], 1, "only the healthy row may deliver: {out}");
    assert_eq!(out["delivered"][0]["message"], "healthy row");

    // The healthy row is consumed; the corrupt ones are skipped — not consumed,
    // not cancelled, still owed to an operator.
    let row = repo.get(&healthy).await.unwrap().unwrap();
    assert_eq!(row.status, "delivered");
    assert_eq!(row.delivered_count, 1);
    for id in &corrupt_ids {
        let row = repo
            .get(id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("corrupt row {id} disappeared"));
        assert_eq!(
            row.status, "pending",
            "corrupt row {id} was not left pending: {}",
            row.message
        );
        assert_eq!(
            row.delivered_count, 0,
            "corrupt row {id} was counted as delivered: {}",
            row.message
        );
    }

    // Nor do they wedge the next check: it runs, and delivers nothing new.
    let out2: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out2["count"], 0);
}

/// Coalescing, counted exactly and without sleeping.
///
/// The wall-clock variant below can only promise `missed_occurrences >= 1` after
/// 130 s — a run that missed one fire satisfies it, so it cannot see an
/// off-by-one. Back-dating a stored row to a whole schedule period makes the
/// arithmetic checkable instead: the counted window is every fire strictly after
/// the stored `next_due_at` up to the check's `now`, and the `next_due_at` the
/// response promises is the first fire after that, so
/// `missed == (next - due) / period - 1` — an identity that pins the count to
/// the schedule without depending on where inside the period the check lands.
///
/// The period is an hour rather than a minute so that the immediate recheck
/// cannot straddle the freshly advanced `next_due_at` (which would be a genuine
/// second delivery, not a bug).
#[tokio::test]
async fn check_coalescing_counts_every_fire_since_the_stored_due_time() {
    let server = setup().await;
    let repo = ReminderRepo::new(server.db.inner());

    let now = Utc::now();
    let hour = now
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap();
    let due = hour - chrono::Duration::hours(10);

    let mut row = raw_row("hourly sweep", Some(due));
    row.schedule_kind = schedule_kind::CRON.to_string();
    row.cron_expr = Some("0 0 * * * *".to_string());
    repo.create(&row).await.unwrap();

    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["recurring"], true);
    assert_eq!(out["delivered"][0]["schedule"], "cron '0 0 * * * *'");

    let field_dt = |field: &str| {
        DateTime::parse_from_rfc3339(out["delivered"][0][field].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc)
    };
    assert_eq!(field_dt("due_at"), due, "must report the time it was due");
    let next = field_dt("next_due_at");

    // Both instants are period-aligned, so the division is exact. `>= 11` keeps
    // the window real (10 back-dated hours plus the one `now` sits in, and a
    // check that crossed an hour boundary adds another) — without it the
    // identity would also be satisfied by a window of nothing.
    let periods = (next - due).num_hours();
    assert!(
        periods >= 11,
        "back-dated window collapsed to {periods} periods"
    );
    assert_eq!(
        out["delivered"][0]["missed_occurrences"].as_u64().unwrap(),
        (periods - 1) as u64,
        "coalesced count must be every fire between the stored due time and the delivered one"
    );

    // The advanced row is in the future: an immediate recheck delivers nothing.
    let out2: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out2["count"], 0);
}

/// A configured window must actually gate escalation.
///
/// `escalation_hours: 0` (the test above) short-circuits the comparison — it
/// cannot catch a build that escalates every project reminder. `escalation_hours`
/// is whole hours, so 1 is the smallest workable nonzero window, and the
/// reminders are created already on the far side of it: the check only reads
/// `now - next_due_at`, so back-dating exercises both sides of the boundary
/// without sleeping for an hour.
#[tokio::test]
async fn check_escalates_after_configured_window() {
    const HOUR_SECS: i64 = 3600;
    let server = setup_with_escalation(tz(), 1).await;

    // Ages far enough past (and short of) the boundary that a few seconds of
    // test overhead can't flip either case.
    for (side, age_secs) in [("past", HOUR_SECS + 70), ("inside", HOUR_SECS - 100)] {
        let mut p = once_params_overdue(&format!("{side} window reminder"), age_secs);
        p.target_project = Some("infra".to_string());
        server.do_set_reminder(p).await.unwrap();
    }

    // Only the one past the 1 h window escalates into an unrelated context.
    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_check_reminders(check(Some("elsewhere")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["message"], "past window reminder");
    assert_eq!(out["delivered"][0]["escalated"], true);
    assert_eq!(out["delivered"][0]["target"], "project:infra");

    // The one still inside the window was held, not dropped: it reaches its own
    // project as a plain match.
    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_check_reminders(check(Some("infra")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["message"], "inside window reminder");
    assert_eq!(out["delivered"][0]["escalated"], false);
}

/// A recurring reminder advances instead of being consumed, and the occurrences
/// that elapsed while nobody consumed it are coalesced into one delivery.
///
/// End-to-end wall-clock proof of the `set_reminder` → `check_reminders` path
/// (the computed count above covers the arithmetic). The sleep has to cover the
/// worst-case wait for the stored `next_due_at` (up to a whole period) *plus*
/// one more full period — one period plus slack would land between two
/// boundaries depending on where in the minute the reminder was set, making
/// `missed_occurrences >= 1` a coin flip.
#[tokio::test]
#[ignore]
async fn check_recurring_advances_and_coalesces() {
    const PERIOD_SECS: u64 = 60; // every-minute cron
    let server = setup().await;
    server
        .do_set_reminder(cron_params("minutely sweep", "* * * * *"))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_secs(2 * PERIOD_SECS + 10)).await;
    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["delivered"][0]["recurring"], true);
    assert_eq!(out["delivered"][0]["schedule"], "cron '0 * * * * *'");
    // ≥1 occurrence elapsed since set; missed = occurrences between stored
    // next_due_at and now, excluding the delivered one
    assert!(out["delivered"][0]["missed_occurrences"].as_u64().unwrap() >= 1);
    assert!(out["delivered"][0]["next_due_at"].is_string());

    // next_due_at advanced into the future: immediate recheck delivers nothing
    let out2: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(out2["count"], 0);
}

// --- list_reminders / cancel_reminder: management views ---

fn list_filter(status: Option<&str>, target_project: Option<&str>) -> ListRemindersParams {
    ListRemindersParams {
        status: status.map(str::to_string),
        target_project: target_project.map(str::to_string),
        limit: None,
        offset: None,
    }
}

/// Parse a tool response the way a client would, so a failed assertion shows the
/// whole payload rather than just the field that was wrong.
async fn list_json(server: &AlexandriaServer, params: ListRemindersParams) -> serde_json::Value {
    let out = server.do_list_reminders(params).await.unwrap();
    serde_json::from_str(&out).unwrap()
}

/// The management path end to end: what is scheduled, and taking one back again.
///
/// `list` defaults to pending (the status a user means when they ask without
/// qualifying) and cancel is a soft cancel, so the row must stay discoverable
/// under `cancelled` rather than vanish — it is the only record that the reminder
/// was ever set.
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
    let list = list_json(&server, list_filter(None, None)).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["timezone"], "UTC", "the envelope names the zone");
    assert_eq!(list["reminders"][0]["message"], "water plants");
    assert_eq!(
        list["reminders"][0]["schedule"],
        "one-shot at 2030-01-01 12:00 UTC"
    );
    assert_eq!(list["reminders"][0]["id"].as_str().unwrap(), id);
    assert_eq!(list["reminders"][0]["target"], "global");
    assert_eq!(list["reminders"][0]["status"], "pending");
    // Every datetime in a reminder response keeps the Z-suffixed UTC spelling
    // (`to_rfc3339()` would write `+00:00`), including the set-time stamp.
    assert_eq!(list["reminders"][0]["next_due_at"], "2030-01-01T12:00:00Z");
    // The UTC time is paired with the local rendering the user thinks in, and a
    // one-shot says so: `schedule` renders one-shots in UTC but patterns in
    // local wall clock, so neither field alone answers "when does this fire?".
    assert_eq!(
        list["reminders"][0]["next_due_at_local"],
        "2030-01-01 12:00 UTC"
    );
    assert_eq!(list["reminders"][0]["recurring"], false);
    assert!(
        list["reminders"][0]["provenance"]["project"].is_null(),
        "an unset provenance is null, not an invented value: {}",
        list["reminders"][0]["provenance"]
    );
    assert!(
        list["reminders"][0]["created_at"]
            .as_str()
            .is_some_and(|s| s.ends_with('Z')),
        "created_at must be Z-suffixed: {}",
        list["reminders"][0]["created_at"]
    );
    assert_eq!(list["reminders"][0]["delivered_count"], 0);
    assert!(list["reminders"][0]["last_delivered_at"].is_null());

    // cancel
    let cancel: serde_json::Value = serde_json::from_str(
        &server
            .do_cancel_reminder(CancelReminderParams { id: id.clone() })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cancel["status"], "ok");
    assert_eq!(cancel["id"].as_str().unwrap(), id);

    // gone from pending, visible under cancelled
    let list2 = list_json(&server, list_filter(None, None)).await;
    assert_eq!(list2["count"], 0);
    let list3 = list_json(&server, list_filter(Some("cancelled"), None)).await;
    assert_eq!(list3["count"], 1);
    assert_eq!(list3["reminders"][0]["message"], "water plants");

    // cancel unknown id -> error, not panic
    let err = server
        .do_cancel_reminder(CancelReminderParams {
            id: "reminder:nonexistent".to_string(),
        })
        .await;
    let err = err.expect_err("an unknown id must not read as a successful cancel");
    assert!(
        err.to_string().contains("Reminder not found"),
        "the error must name the failure: {err:#}"
    );

    // The recurring half of the parity, plus provenance: a weekly reminder set
    // from a project and session must report what it is (`recurring`), when it
    // fires in the reader's own terms (`next_due_at_local`, matching what `set`
    // promised), and where it came from — every one of those is a field another
    // reminder response already carries, so the management view dropping them was
    // an omission, not a narrower design.
    let mut standup = weekly_params("standup");
    standup.prov_project = Some("dashboard".to_string());
    standup.session_id = Some("session-7".to_string());
    let set: serde_json::Value =
        serde_json::from_str(&server.do_set_reminder(standup).await.unwrap()).unwrap();

    let pending = list_json(&server, list_filter(None, None)).await;
    assert_eq!(
        pending["count"], 1,
        "the cancelled row stays out of pending"
    );
    assert_eq!(pending["timezone"], "UTC");
    let row = &pending["reminders"][0];
    assert_eq!(row["message"], "standup");
    assert_eq!(row["schedule"], "every Friday at 09:00");
    assert_eq!(row["recurring"], true);
    assert_eq!(row["next_due_at"], set["next_due_at"]);
    assert_eq!(row["next_due_at_local"], set["next_due_at_local"]);
    assert_eq!(row["provenance"]["project"], "dashboard");
    assert_eq!(row["provenance"]["session_id"], "session-7");
}

/// The filter semantics the tool description promises: `pending` is the default,
/// `all` is the only spelling of "no filter", an unknown status matches nothing
/// (rather than erroring at the user), and the project filter is byte-exact and
/// combines with the status filter.
#[tokio::test]
async fn list_status_and_project_filters() {
    let server = setup().await;
    // A pending global one-shot, a pending project reminder, and a delivered one.
    let delivered_id = server
        .do_set_reminder(once_params_overdue("pay rent", 60))
        .await
        .unwrap();
    let delivered_id = serde_json::from_str::<serde_json::Value>(&delivered_id).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.do_check_reminders(check(None)).await.unwrap();
    server
        .do_set_reminder(weekly_params("standup"))
        .await
        .unwrap();
    server
        .do_set_reminder(once_params("someday", "2030-01-01T12:00:00Z"))
        .await
        .unwrap();

    let pending = list_json(&server, list_filter(None, None)).await;
    assert_eq!(pending["count"], 2, "the consumed row leaves pending");
    assert!(
        pending["reminders"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "pending"),
        "the default filter must not mix statuses: {pending}"
    );

    let all = list_json(&server, list_filter(Some("all"), None)).await;
    assert_eq!(all["count"], 3, "`all` means no filter");
    let one_delivered = all["reminders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == delivered_id.as_str())
        .unwrap_or_else(|| panic!("delivered row missing from {all}"));
    assert_eq!(one_delivered["status"], "delivered");
    // Delivery history survives: the list is where a user sees "fired 1 time".
    assert_eq!(one_delivered["delivered_count"], 1);
    assert!(
        one_delivered["last_delivered_at"]
            .as_str()
            .is_some_and(|s| s.ends_with('Z'))
    );
    // Consumption retires a one-shot by flipping `status`, not by clearing
    // `next_due_at` (due-ness is `status = 'pending' AND next_due_at <= now`), so
    // the list still reports the time it fired at — which is the answer to "when
    // did that go off?".
    assert!(one_delivered["next_due_at"].is_string());

    let only_delivered = list_json(&server, list_filter(Some("delivered"), None)).await;
    assert_eq!(only_delivered["count"], 1);

    // A status outside the schema's set matches no row: an empty list is more
    // useful to a client than an error to explain.
    let bogus = list_json(&server, list_filter(Some("archived"), None)).await;
    assert_eq!(bogus["count"], 0);

    // Project filter: byte-exact (the same rule delivery uses), and combinable.
    let projects = list_json(&server, list_filter(None, Some("alexandria"))).await;
    assert_eq!(projects["count"], 1);
    assert_eq!(projects["reminders"][0]["message"], "standup");
    assert_eq!(projects["reminders"][0]["target"], "project:alexandria");
    let mismatch = list_json(&server, list_filter(None, Some("Alexandria"))).await;
    assert_eq!(mismatch["count"], 0, "project matching is byte-exact");
    let delivered_in_project =
        list_json(&server, list_filter(Some("delivered"), Some("infra"))).await;
    assert_eq!(delivered_in_project["count"], 0);
}

/// Cancelling is unconditional and idempotent, which is what makes it safe for a
/// client to retry: a reminder that already fired, or already was cancelled, is
/// in a state the caller could accept, so it is a success rather than an error.
/// What cancel must not do is undo delivery history, touch a different row, or
/// re-stamp the retirement time of a row that is already cancelled.
#[tokio::test]
async fn cancel_is_soft_and_idempotent() {
    let server = setup().await;
    let repo = ReminderRepo::new(server.db.inner());
    let fired = server
        .do_set_reminder(once_params_overdue("renew cert", 60))
        .await
        .unwrap();
    let fired = serde_json::from_str::<serde_json::Value>(&fired).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.do_check_reminders(check(None)).await.unwrap();
    let other = server
        .do_set_reminder(once_params("untouched", "2030-01-01T12:00:00Z"))
        .await
        .unwrap();
    let other = serde_json::from_str::<serde_json::Value>(&other).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let cancel: serde_json::Value = serde_json::from_str(
        &server
            .do_cancel_reminder(CancelReminderParams { id: fired.clone() })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cancel["status"], "ok", "a delivered row is cancellable");

    // The first cancel is what stamps the retirement time.
    let first_cancelled_at = {
        let row = repo.get(&fired).await.unwrap().unwrap();
        assert_eq!(row.status, "cancelled");
        row.cancelled_at.expect("a cancel must stamp cancelled_at")
    };

    // Repeating it is still a success — a client may retry — but it has to be a
    // true no-op: `ReminderRepo::cancel` writes `cancelled_at` unconditionally,
    // so a repeat that reached the repo would move the stamp while reporting the
    // same "ok".
    let again: serde_json::Value = serde_json::from_str(
        &server
            .do_cancel_reminder(CancelReminderParams { id: fired.clone() })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(again["status"], "ok");
    assert_eq!(again["id"].as_str().unwrap(), fired);

    let row = repo.get(&fired).await.unwrap().unwrap();
    assert_eq!(row.status, "cancelled");
    assert_eq!(row.delivered_count, 1, "cancel must not rewind delivery");
    assert_eq!(
        row.cancelled_at,
        Some(first_cancelled_at),
        "a repeat cancel must not re-stamp cancelled_at"
    );

    // Only the named row is touched, and the cancelled one stays listable.
    let other_row = repo.get(&other).await.unwrap().unwrap();
    assert_eq!(other_row.status, "pending");
    assert_eq!(
        list_json(&server, list_filter(Some("cancelled"), None)).await["count"],
        1
    );

    // Nor does the cancelled row come back: the future pending one is not due,
    // so a later check finds nothing to deliver.
    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(
        out["count"], 0,
        "a cancelled reminder must never deliver: {out}"
    );
}

/// A row the delivery loop refuses to consume must still appear in the listing:
/// `list` is how an operator finds such a row to cancel it, so failing the whole
/// response — or dropping the row — would hide exactly the backlog that
/// `check_skips_corrupt_rows_and_still_delivers_the_healthy_one` leaves behind.
/// The failure is rendered into that one row's `schedule`, per row, and every
/// other row keeps its real one.
#[tokio::test]
async fn list_surfaces_corrupt_row_with_invalid_schedule_marker() {
    let server = setup().await;
    let repo = ReminderRepo::new(server.db.inner());

    // A `once` row with no `due_at`: the shape `spec_from_reminder` rejects. It
    // is due, so a check skips it — which is why the listing has to keep it.
    let past = Utc::now() - chrono::Duration::minutes(5);
    let corrupt = repo
        .create(&raw_row("corrupt row", Some(past)))
        .await
        .unwrap();
    let healthy = server
        .do_set_reminder(once_params("healthy row", "2030-01-01T12:00:00Z"))
        .await
        .unwrap();
    let healthy = serde_json::from_str::<serde_json::Value>(&healthy).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let list = list_json(&server, list_filter(None, None)).await;
    assert_eq!(list["count"], 2, "corrupt row must still be listed: {list}");
    let row = |message: &str| {
        list["reminders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["message"] == message)
            .unwrap_or_else(|| panic!("{message} row missing from {list}"))
            .clone()
    };

    let corrupt_row = row("corrupt row");
    let corrupt_schedule = corrupt_row["schedule"].as_str().unwrap();
    assert!(
        corrupt_schedule.starts_with("<invalid schedule:"),
        "corrupt row must name its own failure: {corrupt_schedule}"
    );
    assert!(
        corrupt_schedule.contains("once reminder missing due_at"),
        "marker must carry the engine's reason: {corrupt_schedule}"
    );
    assert_eq!(corrupt_row["id"].as_str().unwrap(), corrupt);
    assert_eq!(corrupt_row["status"], "pending");

    // The neighbour is untouched by the row that failed beside it.
    let healthy_row = row("healthy row");
    assert_eq!(
        healthy_row["schedule"].as_str().unwrap(),
        "one-shot at 2030-01-01 12:00 UTC"
    );
    assert_eq!(healthy_row["id"].as_str().unwrap(), healthy);

    // And a corrupt row is still manageable: cancelling it by the listed id
    // retires it without touching the healthy row.
    let cancel: serde_json::Value = serde_json::from_str(
        &server
            .do_cancel_reminder(CancelReminderParams {
                id: corrupt.clone(),
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cancel["status"], "ok");
    assert_eq!(
        repo.get(&corrupt).await.unwrap().unwrap().status,
        "cancelled"
    );
    assert_eq!(repo.get(&healthy).await.unwrap().unwrap().status, "pending");
}

/// The passive safety net: `retrieve_memories` also carries a read-only
/// `due_reminders` array, so an agent that never calls `check_reminders` still
/// sees that something came due. Piggybacking must never consume — the whole
/// delivery model (claim-based `record_delivery`, escalation, occurrence
/// coalescing) lives in `check_reminders` alone, so a read that advanced or
/// flipped a row would silently steal the reminder from the next check. The
/// follow-up check therefore has to deliver the same row.
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
    let chk: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(chk["count"], 1);
}

/// The same safety net on the recall path: `recall` carries `due_reminders` in
/// both of its modes, and this is the mode an agent reaches for first (no scope
/// handle yet). Read-only for the same reason as `retrieve_memories` — the check
/// afterwards must still see the row as due.
#[tokio::test]
async fn piggyback_recall_broad_lists_due_reminders_without_consuming() {
    let server = setup().await;
    server
        .do_set_reminder(once_params("nag me too", "2020-01-01T12:00:00Z"))
        .await
        .unwrap();
    server
        .do_store_memory(alexandria_mcp::tools::StoreMemoryParams {
            content: "the search index is built with tantivy".to_string(),
            tags: None,
            session_id: None,
        })
        .await
        .unwrap();

    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_recall(alexandria_mcp::tools::RecallParams {
                query: "search index".to_string(),
                scope_handle: None,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out["mode"], "broad", "no scope handle is the broad pass");
    assert_eq!(out["due_reminders"].as_array().unwrap().len(), 1);
    assert_eq!(out["due_reminders"][0]["message"], "nag me too");

    // NOT consumed — check_reminders still delivers it
    let chk: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(chk["count"], 1);
}

/// The path a due reminder is most likely to ride in on: a lookup that finds
/// nothing at all. `do_retrieve_memories` returns early when no fact matches,
/// so if that branch forgot the piggyback the safety net would disappear
/// exactly when the agent has nothing else on its mind. `results` is empty and
/// `due_reminders` is not, and the row is still pending afterwards.
#[tokio::test]
async fn piggyback_empty_retrieve_still_lists_due_reminders() {
    let server = setup().await;
    server
        .do_set_reminder(once_params("nag on empty search", "2020-01-01T12:00:00Z"))
        .await
        .unwrap();

    // No memory is ever stored, so the query matches nothing and the response
    // comes from the empty-results early return rather than the ranked path.
    let out = server
        .do_retrieve_memories(alexandria_mcp::tools::RetrieveMemoriesParams {
            query: "nothing about this at all".to_string(),
            limit: Some(5),
            session_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        out["results"].as_array().unwrap().len(),
        0,
        "a server with no facts must return no results: {out}"
    );
    assert_eq!(out["due_reminders"].as_array().unwrap().len(), 1);
    assert_eq!(out["due_reminders"][0]["message"], "nag on empty search");

    // Read-only: the empty-results branch consumed nothing either.
    let chk: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(chk["count"], 1);
}

// --- fixes from the pre-merge review: contract and race coverage ---

/// The one field whose entire purpose is to be read. Before this it was accepted
/// blank, reported `status: ok` with a fire preview, and then delivered an empty
/// bullet — with the client inventing a placeholder to hide it.
#[tokio::test]
async fn set_reminder_rejects_a_blank_message() {
    let server = setup().await;
    for blank in ["", "   ", "\n\t"] {
        let err = server
            .do_set_reminder(once_params(blank, "2030-01-01T15:00:00Z"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("message must not be empty"), "{err}");
    }
    assert_no_rows(&server).await;
}

/// The schema enforces the same thing, because `do_set_reminder` is not the only
/// possible writer (admin and migration SQL exist) and a stored empty message is
/// a delivery of nothing. Whitespace counts as empty for the same reason it does
/// in the tool layer: the row would deliver an invisible bullet.
#[tokio::test]
async fn storage_rejects_an_empty_message_directly() {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();
    let repo = ReminderRepo::new(db.inner());

    for blank in ["", "   "] {
        let stored = repo
            .create(&NewReminder {
                message: blank.to_string(),
                target_project: None,
                prov_project: None,
                prov_session_id: None,
                note: None,
                schedule_kind: schedule_kind::ONCE.to_string(),
                due_at: Some(Utc::now()),
                freq: None,
                time_of_day: None,
                weekdays: Vec::new(),
                day_of_month: None,
                cron_expr: None,
                next_due_at: Some(Utc::now()),
            })
            .await;
        match stored {
            Err(e) => assert!(
                e.to_string().contains("must conform to"),
                "unexpected rejection for {blank:?}: {e}"
            ),
            Ok(id) => panic!("a blank message {blank:?} was stored as {id}"),
        }
    }
}

/// `cron` ANDs day-of-month with day-of-week where Vixie cron ORs them, so
/// `0 9 13 * FRI` means "Friday the 13th only". The expression is valid and does
/// fire, so the only place to catch it is the set-time warning the response
/// already carries for the numeric-dow trap.
#[tokio::test]
async fn set_reminder_warns_when_dom_and_dow_conflict() {
    let server = setup().await;
    let out: serde_json::Value = serde_json::from_str(
        &server
            .do_set_reminder(cron_params("friday the thirteenth", "0 9 13 * FRI"))
            .await
            .unwrap(),
    )
    .unwrap();
    let warning = out["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("ANDed"), "{warning}");
    assert!(
        warning.contains("two reminders"),
        "{warning} must offer the fix"
    );

    // The same warning channel must stay quiet for expressions where the
    // intersection is meaningless — otherwise it is noise an agent learns to skip.
    for benign in ["0 9 * * FRI", "0 9 13 * *"] {
        let out: serde_json::Value = serde_json::from_str(
            &server
                .do_set_reminder(cron_params("x", benign))
                .await
                .unwrap(),
        )
        .unwrap();
        let warning = out["warning"].as_str().unwrap_or_default();
        assert!(
            !warning.contains("ANDed"),
            "{benign} should not warn: {warning}"
        );
    }
}

/// The guarantee `do_check_reminders` documents is about *concurrent* consumers,
/// and until now it was only ever tested by re-issuing a stale claim in sequence.
/// That pins the WHERE clause; it does not exercise the case the guarantee is
/// written for — two checks that both see the row as due. The pi companion plus an
/// agent calling the tool manually is exactly that shape on HTTP.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_checks_deliver_a_due_row_exactly_once() {
    let server = setup().await;
    let id = server
        .do_set_reminder(once_params(
            "only one of you should get this",
            &Utc::now().to_rfc3339(),
        ))
        .await
        .unwrap();
    let id: serde_json::Value = serde_json::from_str(&id).unwrap();
    let id = id["id"].as_str().unwrap().to_string();

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let server = server.clone();
        tasks.push(tokio::spawn(async move {
            server.do_check_reminders(check(None)).await.unwrap()
        }));
    }
    let mut winners = 0;
    for t in tasks {
        let out: serde_json::Value = serde_json::from_str(&t.await.unwrap()).unwrap();
        if out["delivered"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == serde_json::Value::String(id.clone()))
        {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "four concurrent checks delivered the same row {winners} times"
    );

    let row = ReminderRepo::new(server.db.inner())
        .get(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.delivered_count, 1,
        "a duplicate claim must not bump delivered_count"
    );
    assert_eq!(row.status, "delivered");
}

/// The documented contract of the piggyback — "up to 5, oldest-due first" — was
/// nowhere pinned, and the cap was applied in Rust after reading every due row.
#[tokio::test]
async fn piggyback_caps_at_five_oldest_due_first() {
    let server = setup().await;
    let repo = ReminderRepo::new(server.db.inner());
    let base = Utc::now() - chrono::Duration::hours(1);
    for i in 0..8 {
        repo.create(&NewReminder {
            message: format!("due {i}"),
            target_project: None,
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: schedule_kind::ONCE.to_string(),
            due_at: Some(base + chrono::Duration::minutes(i)),
            freq: None,
            time_of_day: None,
            weekdays: Vec::new(),
            day_of_month: None,
            cron_expr: None,
            next_due_at: Some(base + chrono::Duration::minutes(i)),
        })
        .await
        .unwrap();
    }

    let out = server
        .do_retrieve_memories(alexandria_mcp::tools::RetrieveMemoriesParams {
            query: "anything".to_string(),
            limit: Some(5),
            session_id: None,
        })
        .await
        .unwrap();
    let due = out["due_reminders"].as_array().unwrap();
    assert_eq!(due.len(), 5, "the documented cap is 5, got {due:?}");
    let msgs: Vec<&str> = due.iter().map(|d| d["message"].as_str().unwrap()).collect();
    assert_eq!(
        msgs,
        vec!["due 0", "due 1", "due 2", "due 3", "due 4"],
        "oldest-due first"
    );
}

/// `list_reminders` is the tool an agent calls to review history, and history is
/// append-only (delivered and cancelled rows are kept deliberately). Unbounded,
/// the recommended flow serialises every reminder ever made into one context.
#[tokio::test]
async fn list_reminders_pages_and_reports_truncation() {
    let server = setup().await;
    let repo = ReminderRepo::new(server.db.inner());
    let base = Utc::now() + chrono::Duration::hours(1);
    for i in 0..5 {
        repo.create(&NewReminder {
            message: format!("row {i}"),
            target_project: None,
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: schedule_kind::ONCE.to_string(),
            due_at: Some(base + chrono::Duration::minutes(i)),
            freq: None,
            time_of_day: None,
            weekdays: Vec::new(),
            day_of_month: None,
            cron_expr: None,
            next_due_at: Some(base + chrono::Duration::minutes(i)),
        })
        .await
        .unwrap();
    }

    let page1 = list_json(
        &server,
        ListRemindersParams {
            status: None,
            target_project: None,
            limit: Some(2),
            offset: Some(0),
        },
    )
    .await;
    assert_eq!(page1["count"], 2);
    assert_eq!(page1["truncated"], true);
    assert_eq!(page1["next_offset"], 2);

    let last = list_json(
        &server,
        ListRemindersParams {
            status: None,
            target_project: None,
            limit: Some(2),
            offset: Some(4),
        },
    )
    .await;
    assert_eq!(last["count"], 1);
    assert_eq!(last["truncated"], false);
    assert_eq!(last["next_offset"], serde_json::Value::Null);

    // An absurd page size is clamped rather than honoured: `limit` is client input.
    let huge = list_json(
        &server,
        ListRemindersParams {
            status: None,
            target_project: None,
            limit: Some(i64::MAX),
            offset: None,
        },
    )
    .await;
    assert_eq!(
        huge["limit"],
        alexandria_mcp::server::MAX_LIST_REMINDERS_LIMIT
    );
}

/// A capped count published as an exact number is wrong where a user looks: the
/// delivery payload has to carry that it stopped early.
#[tokio::test]
async fn check_reminders_reports_a_saturated_missed_count() {
    let server = setup().await;
    let repo = ReminderRepo::new(server.db.inner());
    // Every second, left overdue for three hours: 10 800 elapsed fires against
    // the engine's 10 000 bound.
    let expr = alexandria_engine::reminders::normalize_cron("* * * * * *").unwrap();
    let overdue = Utc::now() - chrono::Duration::hours(3);
    let id = repo
        .create(&NewReminder {
            message: "abandoned metronome".to_string(),
            target_project: None,
            prov_project: None,
            prov_session_id: None,
            note: None,
            schedule_kind: schedule_kind::CRON.to_string(),
            due_at: None,
            freq: None,
            time_of_day: None,
            weekdays: Vec::new(),
            day_of_month: None,
            cron_expr: Some(expr),
            next_due_at: Some(overdue),
        })
        .await
        .unwrap();

    let out: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    let row = out["delivered"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == serde_json::Value::String(id.clone()))
        .expect("the overdue row is delivered");
    assert_eq!(row["missed_occurrences"], 10_000);
    assert_eq!(
        row["missed_occurrences_saturated"], true,
        "the count hit the engine's cap and must say so"
    );
}

/// The `due_reminders` piggyback is informational, so its contract is "up to 5,
/// oldest-due first" — and neither half was pinned by any test, while the read
/// itself was unbounded (every due row deserialized on every prompt to show five).
/// The sample also has to survive an undeliverable row: NULL `next_due_at` sorts
/// first, so a single admin-written row of that shape used to occupy a slot and
/// report `"due_at": null` on every memory lookup forever.
#[tokio::test]
async fn piggyback_sample_is_bounded_ordered_and_hides_undeliverable_rows() {
    let server = setup().await;
    let repo = ReminderRepo::new(server.db.inner());
    let past = Utc::now() - chrono::Duration::hours(2);

    // Undeliverable first: NULL due time (oldest-sorted) and a cron row the engine
    // cannot reconstruct. Both are `list_due` material on purpose.
    let mut no_due = raw_row("admin row with no due time", None);
    no_due.due_at = Some(past);
    repo.create(&no_due).await.unwrap();
    let mut junk = raw_row("junk cron row", Some(past));
    junk.schedule_kind = schedule_kind::CRON.to_string();
    junk.cron_expr = Some("99 99 99 99 99".to_string());
    repo.create(&junk).await.unwrap();

    for i in 0..7 {
        let due = past + chrono::Duration::minutes(i);
        // `raw_row` leaves `due_at` empty on purpose (it is the unreadable-`once`
        // fixture); a row the sample has to keep needs a readable schedule.
        let mut row = raw_row(&format!("real {i}"), Some(due));
        row.due_at = Some(due);
        repo.create(&row).await.unwrap();
    }

    let out = server
        .do_retrieve_memories(alexandria_mcp::tools::RetrieveMemoriesParams {
            query: "anything".to_string(),
            limit: Some(3),
            session_id: None,
        })
        .await
        .unwrap();
    let due = out["due_reminders"].as_array().unwrap();
    assert_eq!(due.len(), 5, "the documented cap is 5: {due:?}");
    let msgs: Vec<&str> = due
        .iter()
        .map(|d| d["message"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        msgs,
        vec!["real 0", "real 1", "real 2", "real 3", "real 4"],
        "oldest-due first, and undeliverable rows must not eat slots"
    );
    assert!(
        due.iter().all(|d| d["due_at"].is_string()),
        "no null due times in the sample: {due:?}"
    );

    // Read-only: the same rows are still owed to `check_reminders`.
    let chk: serde_json::Value =
        serde_json::from_str(&server.do_check_reminders(check(None)).await.unwrap()).unwrap();
    assert_eq!(chk["count"], 7, "the sample consumed nothing");
}
