use std::sync::Arc;

use alexandria_mcp::AlexandriaServer;
use alexandria_pipeline::embedding::CandleProvider;
use alexandria_storage::repos::MemoryRepo;
use alexandria_storage::{schema, Database};

async fn setup() -> (AlexandriaServer, Arc<Database>) {
    let db = Database::connect_embedded().await.unwrap();
    schema::bootstrap(db.inner()).await.unwrap();

    let embedding = CandleProvider::new("sentence-transformers/all-MiniLM-L6-v2", "cpu")
        .await
        .unwrap();

    let db = Arc::new(db);
    let server = AlexandriaServer::new(db.clone(), Arc::new(embedding), 0.75, 86400.0);

    (server, db)
}

#[tokio::test]
async fn test_full_flow_store_and_retrieve() {
    let (server, _db) = setup().await;

    // Store memories about two topics
    let auth_memories = vec![
        "OAuth tokens expire after 7 days",
        "JWT signing uses RS256 algorithm",
        "Refresh tokens are stored in httponly cookies",
    ];
    let db_memories = vec![
        "PostgreSQL indexes speed up read queries",
        "Database migrations should be idempotent",
    ];

    for content in &auth_memories {
        let params = alexandria_mcp::tools::StoreMemoryParams {
            content: content.to_string(),
            tags: Some(vec!["auth".to_string()]),
            session_id: None,
        };
        let result = server.do_store_memory(params).await;
        assert!(result.is_ok(), "Failed to store: {content}");
    }

    for content in &db_memories {
        let params = alexandria_mcp::tools::StoreMemoryParams {
            content: content.to_string(),
            tags: Some(vec!["database".to_string()]),
            session_id: None,
        };
        let result = server.do_store_memory(params).await;
        assert!(result.is_ok(), "Failed to store: {content}");
    }

    // Retrieve auth-related memories
    let params = alexandria_mcp::tools::RetrieveMemoriesParams {
        query: "OAuth token expiration".to_string(),
        limit: Some(3),
        session_id: None,
    };
    let result = server.do_retrieve_memories(params).await.unwrap();
    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());
    // First result should be auth-related
    let first_content = results[0]["content"].as_str().unwrap();
    assert!(
        first_content.contains("OAuth")
            || first_content.contains("token")
            || first_content.contains("JWT"),
        "Expected auth-related first result, got: {first_content}"
    );
}

#[tokio::test]
async fn test_delete_excludes_from_search() {
    let (server, db) = setup().await;

    // Store a memory
    let params = alexandria_mcp::tools::StoreMemoryParams {
        content: "temporary secret key is abc123".to_string(),
        tags: None,
        session_id: None,
    };
    let fact_id = server.do_store_memory(params).await.unwrap();

    // Verify it appears in retrieve
    let params = alexandria_mcp::tools::RetrieveMemoriesParams {
        query: "secret key".to_string(),
        limit: Some(5),
        session_id: None,
    };
    let result = server.do_retrieve_memories(params).await.unwrap();
    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());

    // Soft-delete it
    let repo = MemoryRepo::new(db.inner());
    repo.soft_delete_fact(&fact_id).await.unwrap();

    // Should no longer appear
    let params = alexandria_mcp::tools::RetrieveMemoriesParams {
        query: "secret key".to_string(),
        limit: Some(5),
        session_id: None,
    };
    let result = server.do_retrieve_memories(params).await.unwrap();
    let results = result["results"].as_array().unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_empty_database_retrieve() {
    let (server, _db) = setup().await;

    let params = alexandria_mcp::tools::RetrieveMemoriesParams {
        query: "anything".to_string(),
        limit: Some(5),
        session_id: None,
    };
    let result = server.do_retrieve_memories(params).await.unwrap();
    let results = result["results"].as_array().unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_recall_broad_and_focused() {
    let (server, _db) = setup().await;

    // Store a few memories
    for content in &[
        "OAuth uses refresh tokens for session management",
        "CORS headers must include Access-Control-Allow-Origin",
    ] {
        let params = alexandria_mcp::tools::StoreMemoryParams {
            content: content.to_string(),
            tags: None,
            session_id: None,
        };
        server.do_store_memory(params).await.unwrap();
    }

    // Broad recall
    let params = alexandria_mcp::tools::RecallParams {
        query: "authentication".to_string(),
        scope_handle: None,
    };
    let result = server.do_recall(params).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["mode"], "broad");
}

#[tokio::test]
async fn test_update_memory_content() {
    let (server, _db) = setup().await;

    // Store a memory
    let params = alexandria_mcp::tools::StoreMemoryParams {
        content: "Rust is version 1.75".to_string(),
        tags: Some(vec!["rust".to_string()]),
        session_id: None,
    };
    let id = server.do_store_memory(params).await.unwrap();

    // Update content
    let update_params = alexandria_mcp::tools::UpdateMemoryParams {
        id: id.clone(),
        content: Some("Rust is version 1.88".to_string()),
        tags: None,
        confidence: None,
    };
    let result = server.do_update_memory(update_params).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["content_changed"], true);

    // Verify the content was updated
    let repo = MemoryRepo::new(_db.inner());
    let fact = repo.get_fact(&id).await.unwrap().unwrap();
    assert_eq!(fact.content, "Rust is version 1.88");
}

#[tokio::test]
async fn test_update_memory_tags_only() {
    let (server, _db) = setup().await;

    let params = alexandria_mcp::tools::StoreMemoryParams {
        content: "SurrealDB is a database".to_string(),
        tags: Some(vec!["db".to_string()]),
        session_id: None,
    };
    let id = server.do_store_memory(params).await.unwrap();

    // Update tags only — should not trigger re-embedding
    let update_params = alexandria_mcp::tools::UpdateMemoryParams {
        id: id.clone(),
        content: None,
        tags: Some(vec!["database".to_string(), "surrealdb".to_string()]),
        confidence: None,
    };
    let result = server.do_update_memory(update_params).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["content_changed"], false);

    let repo = MemoryRepo::new(_db.inner());
    let fact = repo.get_fact(&id).await.unwrap().unwrap();
    assert_eq!(fact.tags, vec!["database", "surrealdb"]);
}

#[tokio::test]
async fn test_import_document_whole() {
    let (server, _db) = setup().await;

    let params = alexandria_mcp::tools::ImportDocumentParams {
        content: "This is a complete document about Rust programming.".to_string(),
        mode: Some("whole".to_string()),
        chunk_strategy: None,
        tags: Some(vec!["imported".to_string()]),
    };
    let result = server.do_import_document(params).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["count"], 1);
}

#[tokio::test]
async fn test_import_document_chunk_by_heading() {
    let (server, _db) = setup().await;

    let doc = "# Chapter 1\nFirst chapter content.\n\n# Chapter 2\nSecond chapter.\n\n# Chapter 3\nThird chapter.";
    let params = alexandria_mcp::tools::ImportDocumentParams {
        content: doc.to_string(),
        mode: Some("chunk".to_string()),
        chunk_strategy: Some("heading".to_string()),
        tags: Some(vec!["book".to_string()]),
    };
    let result = server.do_import_document(params).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["count"], 3);
}

#[tokio::test]
async fn test_import_document_chunk_by_paragraph() {
    let (server, _db) = setup().await;

    let doc = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n\nFourth paragraph.";
    let params = alexandria_mcp::tools::ImportDocumentParams {
        content: doc.to_string(),
        mode: Some("chunk".to_string()),
        chunk_strategy: Some("paragraph".to_string()),
        tags: None,
    };
    let result = server.do_import_document(params).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["count"], 4);
}
