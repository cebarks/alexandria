use std::sync::Arc;

use alexandria_engine::reminders::{human_readable, spec_from_reminder};
use alexandria_mcp::server::{AlexandriaServer, RemindersSettings};
use alexandria_mcp::tools::{ReminderPatternParams, SetReminderParams};
use alexandria_pipeline::embedding::EmbeddingProvider;
use alexandria_storage::models::schedule_kind;
use alexandria_storage::repos::ReminderRepo;
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
    setup_with_tz(tz()).await
}

/// Non-UTC variant: naive datetimes are interpreted in the configured timezone
/// and responses carry a local rendering alongside the UTC one.
async fn setup_with_tz(tz: chrono_tz::Tz) -> AlexandriaServer {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();
    AlexandriaServer::new(Arc::new(db), Arc::new(StubEmbedding), 0.75, 86400.0)
        .with_reminders_config(RemindersSettings {
            tz,
            escalation_hours: 48,
        })
}

/// A rejected schedule must not leave a row behind: a stored row is a promise of
/// delivery, and `list_due` can never pick up one written with NULL next_due_at.
async fn assert_no_rows(server: &AlexandriaServer) {
    let rows = ReminderRepo::new(server.db.inner())
        .list(None, None)
        .await
        .unwrap();
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
