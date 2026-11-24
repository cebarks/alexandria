use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn imported_chunks_join_only_their_destination_partition_in_the_same_journal_commit() {
    let tools = Tools::hosted().await;
    let alice = tools.owner("import-partition-owner").await;
    let session = tools.handshake(&alice.bearer).await;
    let mut memberships = Vec::new();
    for (index, scope) in ["personal", "project", "project"].into_iter().enumerate() {
        let content = format!("constant-vector import chunk {index}");
        let mut args = json!({"content":content,"mode":"whole","scope":scope});
        if scope == "project" {
            args["project"] = json!(alice.project);
        }
        let result = tools
            .call(&alice.bearer, &session, "import_document", args)
            .await;
        assert!(!refused(&result), "import refused: {}", text_of(&result));
        let rows: Vec<Json> = tools
            .db
            .inner()
            .query("SELECT id FROM fact WHERE content = $content;")
            .bind(("content", content))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(rows.len(), 1, "each whole import creates one live chunk");
        let fact =
            surrealdb::types::RecordId::parse_simple(rows[0]["id"].as_str().unwrap()).unwrap();
        let links: Vec<Json> = tools
            .db
            .inner()
            .query("SELECT in FROM contains_memory WHERE out = $fact;")
            .bind(("fact", fact))
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            links.len(),
            1,
            "an admitted imported chunk lost cluster assignment before its journal commit"
        );
        memberships.push(links[0]["in"].as_str().unwrap().to_string());
    }
    assert_ne!(
        memberships[0], memberships[1],
        "identical vectors in Personal and project cannot share a cluster"
    );
    assert_eq!(
        memberships[1], memberships[2],
        "same-partition chunks must still join their lawful cluster"
    );
    let operations = tools.rows("SELECT id, expected_effects, actual_effects FROM semantic_operation WHERE kind = 'import';").await;
    assert_eq!(operations.len(), 3);
    for operation in operations {
        assert_eq!(operation["expected_effects"], operation["actual_effects"]);
        assert!(
            operation["actual_effects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "contains_memory"),
            "the import journal omitted the membership it wrote: {operation}"
        );
    }
}
