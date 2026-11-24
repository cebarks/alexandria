use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn committed_writes_remain_successful_when_the_activity_stamp_is_refused() {
    for tool in ["store_memory", "import_document"] {
        let tools = Tools::hosted().await;
        let alice = tools.owner("activity-owner").await;
        let session = tools.handshake(&alice.bearer).await;
        tools
            .seed_session("activity-session", "activity-session", &alice)
            .await;
        // Synchronous event failure affects this test's in-memory store only; no production hook.
        tools
            .db
            .inner()
            .query(
                "DEFINE EVENT OVERWRITE refuse_activity ON TABLE session \
            WHEN $event = 'UPDATE' THEN { THROW 'fixture refuses session activity'; };",
            )
            .await
            .unwrap()
            .check()
            .expect("install a real engine refusal");
        let owner = tools.owner_context(&alice).await;
        let sessions = ScopedSessionRepo::new(tools.db.inner(), owner);
        assert!(
            sessions.touch("activity-session").await.is_err(),
            "the activity refusal must really fire"
        );
        let mut args = json!({"content":"a committed memory must not be reported as absent", "session_id":"activity-session", "scope":"personal"});
        if tool == "import_document" {
            args["mode"] = json!("whole");
        }
        let result = tools.call(&alice.bearer, &session, tool, args).await;
        assert_eq!(
            tools.rows("SELECT id FROM fact;").await.len(),
            1,
            "the fixture must reach the committed-write boundary for {tool}: {}",
            text_of(&result)
        );
        assert_eq!(
            tools.rows("SELECT id FROM semantic_operation;").await.len(),
            1
        );
        assert_eq!(
            tools
                .rows("SELECT id FROM contains_session_memory;")
                .await
                .len(),
            1
        );
        assert!(
            !refused(&result),
            "{tool} reported failure after committing its memory: {}",
            text_of(&result)
        );
        tools
            .db
            .inner()
            .query("REMOVE EVENT IF EXISTS refuse_activity ON TABLE session;")
            .await
            .unwrap()
            .check()
            .unwrap();
        sessions
            .touch("activity-session")
            .await
            .expect("the same stamp succeeds after removing only the refusal");
    }
}
