# Cluster Split/Merge Execution Implementation Plan

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Implement the cluster split and merge execution logic that is currently stubbed with TODO comments in the maintenance loop.

**Architecture:** The engine crate already computes *when* to split/merge and *what* the groups/centroids should be. The storage layer needs a few new repo methods (remove member edge, delete cluster, update centroid). The maintenance loop in `main.rs` wires them together. We also fix an existing bug where member counts are passed as `0` to `check_merge`, which corrupts the weighted-centroid calculation.

**Tech Stack:** Rust, SurrealDB (graph relations via `contains_memory` edges), tokio (async)

---

### Task 1: Add missing `ClusterRepo` methods

**Files:**

- Modify: `crates/alexandria-storage/src/repos/cluster_repo.rs`

**Step 1: Write failing tests for the new methods**

Add to the existing `tests` module in `cluster_repo.rs`:

```rust
#[tokio::test]
async fn test_remove_member() {
    let db = Database::connect_embedded().await.unwrap();
    crate::schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());
    let memory_repo = crate::repos::MemoryRepo::new(db.inner());

    let cid = cluster_repo.create(Some("c1"), &[0.1, 0.1]).await.unwrap();
    let f1 = memory_repo.create_fact("f1", 0.5, &[0.1, 0.1], &[]).await.unwrap();
    let f2 = memory_repo.create_fact("f2", 0.5, &[0.2, 0.2], &[]).await.unwrap();
    cluster_repo.add_member(&cid, &f1).await.unwrap();
    cluster_repo.add_member(&cid, &f2).await.unwrap();

    assert_eq!(cluster_repo.get_members(&cid).await.unwrap().len(), 2);

    cluster_repo.remove_member(&cid, &f1).await.unwrap();
    let remaining = cluster_repo.get_members(&cid).await.unwrap();
    assert_eq!(remaining.len(), 1);
}

#[tokio::test]
async fn test_delete_cluster() {
    let db = Database::connect_embedded().await.unwrap();
    crate::schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());
    let memory_repo = crate::repos::MemoryRepo::new(db.inner());

    let cid = cluster_repo.create(Some("doomed"), &[0.1, 0.1]).await.unwrap();
    let f1 = memory_repo.create_fact("f1", 0.5, &[0.1, 0.1], &[]).await.unwrap();
    cluster_repo.add_member(&cid, &f1).await.unwrap();

    cluster_repo.delete(&cid).await.unwrap();

    // Cluster gone
    let all = cluster_repo.list_with_counts().await.unwrap();
    assert!(all.is_empty());
    // Edges gone too — fact should have no cluster
    let memory_repo2 = crate::repos::MemoryRepo::new(db.inner());
    assert!(memory_repo2.cluster_for_fact(&f1).await.unwrap().is_none());
}

#[tokio::test]
async fn test_update_centroid() {
    let db = Database::connect_embedded().await.unwrap();
    crate::schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());

    let cid = cluster_repo.create(Some("c1"), &[1.0, 0.0]).await.unwrap();
    cluster_repo.update_centroid(&cid, &[0.0, 1.0]).await.unwrap();

    // Re-read and verify
    let clusters = cluster_repo.list_with_counts().await.unwrap();
    let (c, _) = &clusters[0];
    assert!((c.centroid[0] - 0.0).abs() < 0.001);
    assert!((c.centroid[1] - 1.0).abs() < 0.001);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-storage test_remove_member test_delete_cluster test_update_centroid -- --nocapture`
Expected: Compilation errors (methods don't exist yet)

**Step 3: Implement the three new methods**

Add to the `impl<'a> ClusterRepo<'a>` block:

```rust
/// Remove a single fact from this cluster (delete the contains_memory edge).
pub async fn remove_member(&self, cluster_id: &str, fact_id: &str) -> Result<()> {
    let from = RecordId::parse_simple(cluster_id)?;
    let to = RecordId::parse_simple(fact_id)?;
    self.db
        .query("DELETE contains_memory WHERE in = $from AND out = $to")
        .bind(("from", from))
        .bind(("to", to))
        .await?
        .check()?;
    Ok(())
}

/// Delete a cluster record and all its contains_memory edges.
pub async fn delete(&self, cluster_id: &str) -> Result<()> {
    let id = RecordId::parse_simple(cluster_id)?;
    // Delete edges first, then the cluster itself
    self.db
        .query("DELETE contains_memory WHERE in = $id")
        .bind(("id", id.clone()))
        .await?
        .check()?;
    self.db
        .query("DELETE type::record($id)")
        .bind(("id", cluster_id.to_string()))
        .await?
        .check()?;
    Ok(())
}

/// Overwrite a cluster's centroid.
pub async fn update_centroid(&self, cluster_id: &str, centroid: &[f32]) -> Result<()> {
    self.db
        .query("UPDATE type::record($id) SET centroid = $centroid")
        .bind(("id", cluster_id.to_string()))
        .bind(("centroid", centroid.to_vec()))
        .await?
        .check()?;
    Ok(())
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-storage -- --nocapture`
Expected: All tests pass

**Step 5: Commit**

```bash
git add crates/alexandria-storage/src/repos/cluster_repo.rs
git commit -m "feat(storage): add remove_member, delete, update_centroid to ClusterRepo"
```

---

### Task 2: Implement split execution in the maintenance loop

**Files:**

- Modify: `crates/alexandria/src/main.rs`

**Step 1: Replace the split TODO with execution logic**

Replace the split TODO block with:

```rust
if let alexandria_engine::clusters::maintenance::MaintenanceAction::Split {
    cluster_id, group_a, group_b, centroid_a, centroid_b,
} = action {
    tracing::info!("Splitting cluster {cluster_id} into two groups ({} / {} members)",
        group_a.len(), group_b.len());

    // Create two new clusters
    let cid_a = match cluster_repo.create(None, &centroid_a).await {
        Ok(id) => id,
        Err(e) => { tracing::warn!("Split: failed to create cluster A: {e}"); continue; }
    };
    let cid_b = match cluster_repo.create(None, &centroid_b).await {
        Ok(id) => id,
        Err(e) => { tracing::warn!("Split: failed to create cluster B: {e}"); continue; }
    };

    // Reassign members — group indices reference `members` vec
    for &idx in &group_a {
        if let Some(fact) = members.get(idx) {
            let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
            let _ = cluster_repo.remove_member(&cid, &fid).await;
            let _ = cluster_repo.add_member(&cid_a, &fid).await;
        }
    }
    for &idx in &group_b {
        if let Some(fact) = members.get(idx) {
            let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
            let _ = cluster_repo.remove_member(&cid, &fid).await;
            let _ = cluster_repo.add_member(&cid_b, &fid).await;
        }
    }

    // Delete the old cluster (now empty)
    let _ = cluster_repo.delete(&cid).await;
    tracing::info!("Split complete: {cluster_id} -> {cid_a}, {cid_b}");
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p alexandria`
Expected: No errors

**Step 3: Commit**

```bash
git add crates/alexandria/src/main.rs
git commit -m "feat: implement cluster split execution in maintenance loop"
```

---

### Task 3: Fix member count bug and implement merge execution

**Files:**

- Modify: `crates/alexandria/src/main.rs`

**Step 1: Fix the merge check to pass real member counts**

The merge pair loop currently has `infos` as `Vec<(String, Vec<f32>)>`. Change it to also carry member count, and pass real counts to `check_merge`.

Replace the merge section to carry member counts and execute the merge:

```rust
// Check pairs for merge — collect member counts alongside centroids
let infos: Vec<_> = clusters.iter().map(|c| {
    let id = c.id.as_ref().map(record_id_to_string).unwrap_or_default();
    let count = cluster_repo.get_members(&id); // resolved below
    (id, c.centroid.clone(), count)
}).collect();
```

Actually, since `get_members` is async, we need to collect differently. Build the info vec with an async loop:

```rust
// Check pairs for merge
let mut infos: Vec<(String, Vec<f32>, usize)> = Vec::new();
for c in &clusters {
    let id = c.id.as_ref().map(record_id_to_string).unwrap_or_default();
    let count = cluster_repo.get_members(&id).await.map(|m| m.len()).unwrap_or(0);
    infos.push((id, c.centroid.clone(), count));
}
for i in 0..infos.len() {
    for j in (i+1)..infos.len() {
        let result = check_merge(
            &infos[i].0, &infos[i].1, infos[i].2,
            &infos[j].0, &infos[j].1, infos[j].2,
            merge_threshold,
        );
        if let alexandria_engine::clusters::maintenance::MergeCheck::Merge {
            keep_id, remove_id, merged_centroid,
        } = result {
            tracing::info!("Merging cluster {remove_id} into {keep_id}");

            // Move all members from removed cluster to kept cluster
            let removed_members = match cluster_repo.get_members(&remove_id).await {
                Ok(m) => m,
                Err(e) => { tracing::warn!("Merge: {e}"); continue; }
            };
            for fact in &removed_members {
                let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
                let _ = cluster_repo.remove_member(&remove_id, &fid).await;
                let _ = cluster_repo.add_member(&keep_id, &fid).await;
            }

            // Update kept cluster's centroid to the weighted merge
            let _ = cluster_repo.update_centroid(&keep_id, &merged_centroid).await;

            // Delete the empty cluster
            let _ = cluster_repo.delete(&remove_id).await;
            tracing::info!("Merge complete: {remove_id} -> {keep_id}");

            // Break out of inner loop — cluster list is stale after mutation
            break;
        }
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p alexandria`
Expected: No errors

**Step 3: Commit**

```bash
git add crates/alexandria/src/main.rs
git commit -m "fix: pass real member counts to check_merge, implement merge execution"
```

---

### Task 4: Integration test for split and merge

**Files:**

- Modify: `crates/alexandria/tests/integration_test.rs` (or create a new test file)

**Step 1: Write an integration test that exercises the storage-level split flow**

Since the maintenance loop runs inside the HTTP server, we test the building blocks directly — the repo methods that the loop calls. This validates the split/merge *mechanics* without needing a running server.

```rust
#[tokio::test]
async fn test_cluster_split_mechanics() {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());
    let memory_repo = MemoryRepo::new(db.inner());

    // Create a cluster with 6 members
    let cid = cluster_repo.create(Some("mixed"), &[0.5, 0.5, 0.0]).await.unwrap();
    let mut fact_ids = Vec::new();
    let embeddings = vec![
        vec![1.0, 0.0, 0.0], vec![0.95, 0.05, 0.0], vec![0.98, 0.02, 0.0],
        vec![0.0, 1.0, 0.0], vec![0.05, 0.95, 0.0], vec![0.02, 0.98, 0.0],
    ];
    for (i, emb) in embeddings.iter().enumerate() {
        let fid = memory_repo.create_fact(&format!("fact{i}"), 0.5, emb, &[]).await.unwrap();
        cluster_repo.add_member(&cid, &fid).await.unwrap();
        fact_ids.push(fid);
    }

    // Verify cohesion check says split
    let members = cluster_repo.get_members(&cid).await.unwrap();
    let member_embeddings: Vec<Vec<f32>> = members.iter().map(|f| f.embedding.clone()).collect();
    let action = check_cohesion(&cid, &[0.5, 0.5, 0.0], &member_embeddings, 0.85);
    let (group_a, group_b, centroid_a, centroid_b) = match action {
        MaintenanceAction::Split { group_a, group_b, centroid_a, centroid_b, .. } => {
            (group_a, group_b, centroid_a, centroid_b)
        }
        MaintenanceAction::Healthy => panic!("Expected split"),
    };

    // Execute split
    let cid_a = cluster_repo.create(None, &centroid_a).await.unwrap();
    let cid_b = cluster_repo.create(None, &centroid_b).await.unwrap();
    for &idx in &group_a {
        let fid = members[idx].id.as_ref().map(record_id_to_string).unwrap_or_default();
        cluster_repo.remove_member(&cid, &fid).await.unwrap();
        cluster_repo.add_member(&cid_a, &fid).await.unwrap();
    }
    for &idx in &group_b {
        let fid = members[idx].id.as_ref().map(record_id_to_string).unwrap_or_default();
        cluster_repo.remove_member(&cid, &fid).await.unwrap();
        cluster_repo.add_member(&cid_b, &fid).await.unwrap();
    }
    cluster_repo.delete(&cid).await.unwrap();

    // Verify: old cluster gone, two new ones exist, all facts accounted for
    let all = cluster_repo.list_with_counts().await.unwrap();
    assert_eq!(all.len(), 2);
    let total_members: usize = all.iter().map(|(_, c)| c).sum();
    assert_eq!(total_members, 6);
}

#[tokio::test]
async fn test_cluster_merge_mechanics() {
    let db = Database::connect_embedded().await.unwrap();
    alexandria_storage::schema::migrate(db.inner()).await.unwrap();
    let cluster_repo = ClusterRepo::new(db.inner());
    let memory_repo = MemoryRepo::new(db.inner());

    // Create two very similar clusters
    let c1 = cluster_repo.create(Some("c1"), &[1.0, 0.0, 0.0]).await.unwrap();
    let c2 = cluster_repo.create(Some("c2"), &[0.98, 0.02, 0.0]).await.unwrap();

    let f1 = memory_repo.create_fact("in c1", 0.5, &[1.0, 0.0, 0.0], &[]).await.unwrap();
    let f2 = memory_repo.create_fact("in c2", 0.5, &[0.98, 0.02, 0.0], &[]).await.unwrap();
    cluster_repo.add_member(&c1, &f1).await.unwrap();
    cluster_repo.add_member(&c2, &f2).await.unwrap();

    // check_merge says merge
    let merge = check_merge(&c1, &[1.0, 0.0, 0.0], 1, &c2, &[0.98, 0.02, 0.0], 1, 0.9);
    let (keep_id, remove_id, merged_centroid) = match merge {
        MergeCheck::Merge { keep_id, remove_id, merged_centroid } => {
            (keep_id, remove_id, merged_centroid)
        }
        MergeCheck::Distinct => panic!("Expected merge"),
    };

    // Execute merge
    let removed_members = cluster_repo.get_members(&remove_id).await.unwrap();
    for fact in &removed_members {
        let fid = fact.id.as_ref().map(record_id_to_string).unwrap_or_default();
        cluster_repo.remove_member(&remove_id, &fid).await.unwrap();
        cluster_repo.add_member(&keep_id, &fid).await.unwrap();
    }
    cluster_repo.update_centroid(&keep_id, &merged_centroid).await.unwrap();
    cluster_repo.delete(&remove_id).await.unwrap();

    // Verify: one cluster, two members
    let all = cluster_repo.list_with_counts().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1, 2);
}
```

**Step 2: Run the integration tests**

Run: `cargo test -p alexandria test_cluster_split_mechanics test_cluster_merge_mechanics -- --nocapture`
Expected: Pass

**Step 3: Run the full test suite**

Run: `cargo test --workspace`
Expected: All tests pass

**Step 4: Commit**

```bash
git add -A
git commit -m "test: add integration tests for cluster split and merge mechanics"
```
