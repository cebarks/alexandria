use super::*;

async fn seed_candidates(tools: &Tools, owner: &Owner, literal: bool) -> (RecordId, RecordId) {
    // Workspace/account handles are ordinary generated IDs; ambiguous destination IDs never pass
    // through the legacy display parser, including during fixture construction and observation.
    let workspace = record(&owner.workspace);
    let account = tools
        .rows(
            "SELECT owner FROM $workspace;",
            vec![("workspace", workspace.clone())],
        )
        .await;
    let account = record(account[0]["owner"].as_str().unwrap());
    let preferred = RecordId::new(
        "cluster",
        if literal {
            "`literal`"
        } else {
            "ordinary-ranked"
        },
    );
    let lookalike = RecordId::new(
        "cluster",
        if literal { "literal" } else { "ordinary-other" },
    );
    for (cluster, key, vector) in [
        (&preferred, "typed-ranked-witness", vec![0.96_f32, 0.28]),
        (&lookalike, "typed-other-witness", vec![0.8_f32, 0.6]),
    ] {
        tools.db.inner().query(
            "CREATE $fact SET content = $content, confidence = 0.5, embedding = $vector, tags = [], \
             account = $account, workspace = $workspace, scope = 'personal'; \
             CREATE $cluster SET label = $content, centroid = $vector, depth = 0, account = $account, workspace = $workspace; \
             RELATE $cluster->contains_memory->$fact;",
        ).bind(("fact", RecordId::new("fact", key))).bind(("cluster", cluster.clone()))
            .bind(("content", key)).bind(("vector", vector)).bind(("account", account.clone()))
            .bind(("workspace", workspace.clone())).await.unwrap().check().unwrap();
    }
    (preferred, lookalike)
}

async fn typed_cluster(tools: &Tools, id: &RecordId) -> Json {
    tools
        .rows("SELECT * FROM $cluster;", vec![("cluster", id.clone())])
        .await
        .remove(0)
}

async fn assert_ranked_writer(import: bool) {
    for literal in [false, true] {
        let tools = Tools::built(vec![]).await;
        let owner = tools.owner("typed-writer-owner").await;
        let session = tools.handshake(&owner.bearer).await;
        let (preferred, lookalike) = seed_candidates(&tools, &owner, literal).await;
        let before_preferred = typed_cluster(&tools, &preferred).await;
        let before_lookalike = typed_cluster(&tools, &lookalike).await;
        let arguments = if import {
            json!({"content":"typed destination writer input", "scope":"personal", "mode":"whole"})
        } else {
            json!({"content":"typed destination writer input", "scope":"personal"})
        };
        let response = tools
            .call(
                &owner,
                &session,
                if import {
                    "import_document"
                } else {
                    "store_memory"
                },
                arguments,
            )
            .await;
        assert!(
            !refused(&response),
            "a lawful typed destination writer was refused: {response}"
        );
        if import {
            assert_eq!(response["structuredContent"]["count"], 1);
        }
        let new_edges = tools.rows(
            "SELECT * FROM contains_memory WHERE out != $preferred_seed AND out != $other_seed;",
            vec![("preferred_seed", RecordId::new("fact", "typed-ranked-witness")),
                 ("other_seed", RecordId::new("fact", "typed-other-witness"))],
        ).await;
        assert_eq!(
            new_edges.len(),
            1,
            "one writer creates exactly one new membership"
        );
        assert_eq!(
            new_edges[0]["in"], before_preferred["id"],
            "literal={literal}, import={import}: mutate the original typed ranked cluster, not its display lookalike"
        );
        let after_preferred = typed_cluster(&tools, &preferred).await;
        assert_eq!(
            typed_cluster(&tools, &lookalike).await,
            before_lookalike,
            "the eligible lookalike stays byte-for-byte unchanged"
        );
        for (actual, expected) in after_preferred["centroid"]
            .as_array()
            .unwrap()
            .iter()
            .zip([0.98, 0.14])
        {
            assert!((actual.as_f64().unwrap() - expected).abs() < 0.00001);
        }
        assert_ne!(after_preferred["centroid"], before_preferred["centroid"]);
        for revision in ["membership_revision", "centroid_revision"] {
            assert_eq!(
                after_preferred[revision].as_i64().unwrap(),
                before_preferred[revision].as_i64().unwrap() + 1
            );
        }
        let operations = tools.operations(&owner.workspace).await;
        assert_eq!(
            operations.len(),
            1,
            "raw fixture seeding creates no journal receipts"
        );
        assert_eq!(
            operations[0]["expected_effects"],
            operations[0]["actual_effects"]
        );
        assert_eq!(
            operations[0]["kind"],
            if import { "import" } else { "store" }
        );
        for (phase, cluster) in [
            ("preimages", &before_preferred),
            ("postimages", &after_preferred),
        ] {
            let mut cluster_images = Vec::new();
            for reference in operations[0][phase].as_array().unwrap() {
                let image = tools.record_row(reference.as_str().unwrap()).await;
                if image["cluster"].is_string() {
                    cluster_images.push(image);
                }
            }
            assert_eq!(
                cluster_images.len(),
                1,
                "exactly one cluster image in {phase}"
            );
            assert_eq!(
                cluster_images[0]["cluster"], before_preferred["id"],
                "{phase} uses original typed journal identity"
            );
            assert_eq!(cluster_images[0]["cluster_centroid"], cluster["centroid"]);
            assert_eq!(
                cluster_images[0]["cluster_membership_revision"],
                cluster["membership_revision"]
            );
            assert_eq!(
                cluster_images[0]["cluster_centroid_revision"],
                cluster["centroid_revision"]
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_destination_store_preserves_original_ranked_identity() {
    assert_ranked_writer(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_destination_import_preserves_original_ranked_identity() {
    assert_ranked_writer(true).await;
}
