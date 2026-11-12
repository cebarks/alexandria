use super::*;
use crate::Database;
use crate::models::auth::PrincipalIds;
use crate::models::identity::OperationId;
use crate::repos::{IdentityRepo, MemoryRepo};
use surrealdb::types::Value;

struct GateFixture {
    db: Database,
    owner: PrincipalIds,
}

impl GateFixture {
    async fn new() -> Self {
        let db = Database::connect_embedded().await.unwrap();
        crate::schema::migrate(db.inner()).await.unwrap();
        let owner = IdentityRepo::new(db.inner())
            .bootstrap_owner(
                "legacy-gate",
                "$argon2id$fixture-gate-padding",
                &OperationId::new("bootstrap"),
            )
            .await
            .unwrap();
        Self { db, owner }
    }

    async fn cluster(&self, label: &str, owned: bool) -> String {
        let repo = ClusterRepo::new(self.db.inner());
        let id = repo.create(Some(label), &[1.0, 0.0]).await.unwrap();
        for i in 0..2 {
            let fact = MemoryRepo::new(self.db.inner())
                .create_fact(&format!("{label}-{i}"), 0.5, &[1.0, 0.0], &[])
                .await
                .unwrap();
            repo.add_member(&id, &fact).await.unwrap();
            if owned {
                self.db.inner().query("UPDATE $fact SET account = $account, workspace = $workspace, scope = 'personal';")
                    .bind(("fact", RecordId::parse_simple(&fact).unwrap()))
                    .bind(("account", self.owner.account.as_record_id().clone()))
                    .bind(("workspace", self.owner.workspace.as_record_id().clone()))
                    .await.unwrap().check().unwrap();
            }
        }
        if owned {
            self.mark_cluster_owned(&id).await;
        }
        id
    }

    async fn mark_cluster_owned(&self, id: &str) {
        self.db
            .inner()
            .query("UPDATE $cluster SET workspace = $workspace;")
            .bind(("cluster", RecordId::parse_simple(id).unwrap()))
            .bind(("workspace", self.owner.workspace.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    async fn observed(&self) -> Vec<Vec<Value>> {
        let mut response = self.db.inner().query(
            "SELECT * FROM fact ORDER BY id; SELECT * FROM cluster ORDER BY id; \
             SELECT * FROM contains_memory ORDER BY id; SELECT * FROM maintenance_log ORDER BY id; \
             SELECT * FROM semantic_operation ORDER BY id; SELECT * FROM semantic_snapshot ORDER BY id;",
        ).await.unwrap().check().unwrap();
        (0..6).map(|index| response.take(index).unwrap()).collect()
    }
}

#[tokio::test]
async fn legacy_maintenance_refuses_owned_split_before_first_write() {
    let f = GateFixture::new().await;
    let id = f.cluster("owned split", true).await;
    let repo = ClusterRepo::new(f.db.inner());
    let members = repo.get_members(&id).await.unwrap();
    assert_eq!(members.len(), 2);
    let before = f.observed().await;
    let result = repo
        .execute_split(&id, &members, &[0], &[1], &[1.0, 0.0], &[1.0, 0.0])
        .await;
    assert!(
        result.is_err(),
        "legacy split must refuse owned data before creating children: {result:?}"
    );
    assert_eq!(
        f.observed().await,
        before,
        "no cluster, edge, corpus, history or audit writes"
    );
}

#[tokio::test]
async fn legacy_maintenance_refuses_owned_merge_on_either_side() {
    for owned_kept in [false, true] {
        let f = GateFixture::new().await;
        let keep = f.cluster("kept", owned_kept).await;
        let remove = f.cluster("removed", !owned_kept).await;
        let before = f.observed().await;
        let result = ClusterRepo::new(f.db.inner())
            .execute_merge(&keep, &remove, &[1.0, 0.0])
            .await;
        assert!(
            result.is_err(),
            "owned side kept={owned_kept}: merge must refuse before moving edges"
        );
        assert_eq!(f.observed().await, before);
    }
}

#[tokio::test]
async fn legacy_maintenance_refuses_mixed_hidden_or_partial_ownership() {
    for marker in ["deleted", "quarantined", "partial-scope"] {
        let f = GateFixture::new().await;
        let id = f.cluster("mixed source", false).await;
        let repo = ClusterRepo::new(f.db.inner());
        let target = repo.get_members(&id).await.unwrap().remove(0).id.unwrap();
        if marker == "partial-scope" {
            f.db.inner()
                .query("UPDATE $fact SET scope = 'personal';")
                .bind(("fact", target))
                .await
                .unwrap()
                .check()
                .unwrap();
        } else {
            f.db.inner().query("UPDATE $fact SET account = $account, workspace = $workspace, scope = 'personal';")
                .bind(("fact", target.clone()))
                .bind(("account", f.owner.account.as_record_id().clone()))
                .bind(("workspace", f.owner.workspace.as_record_id().clone()))
                .await.unwrap().check().unwrap();
            let sql = if marker == "deleted" {
                "UPDATE $fact SET deleted = true;"
            } else {
                "UPDATE $fact SET quarantined_at = d'2020-01-01T00:00:00Z';"
            };
            f.db.inner()
                .query(sql)
                .bind(("fact", target))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
        let members = repo.get_members(&id).await.unwrap();
        let before = f.observed().await;
        let result = repo
            .execute_split(&id, &members, &[0], &[], &[1.0, 0.0], &[1.0, 0.0])
            .await;
        assert!(
            result.is_err(),
            "{marker}: live-only reads must not admit owned retained members"
        );
        assert_eq!(f.observed().await, before);
    }
}

#[tokio::test]
async fn legacy_maintenance_refuses_supplied_owned_or_unknown_split_members() {
    for missing in [false, true] {
        let f = GateFixture::new().await;
        let id = f.cluster("legacy source", false).await;
        let owned = f.cluster("outside owned", true).await;
        let repo = ClusterRepo::new(f.db.inner());
        let mut members = repo.get_members(&id).await.unwrap();
        let mut supplied = repo.get_members(&owned).await.unwrap().remove(0);
        if missing {
            supplied.id = Some(RecordId::new("fact", "missing-gate-input"));
        }
        members.push(supplied);
        let before = f.observed().await;
        let result = repo
            .execute_split(&id, &members, &[0], &[1, 2], &[1.0, 0.0], &[1.0, 0.0])
            .await;
        assert!(
            result.is_err(),
            "missing={missing}: all supplied IDs need a physical ownership check"
        );
        assert_eq!(f.observed().await, before);
    }
}

#[tokio::test]
async fn legacy_maintenance_rechecks_selected_candidate() {
    let f = GateFixture::new().await;
    let id = f.cluster("selected legacy", false).await;
    let repo = ClusterRepo::new(f.db.inner());
    assert!(
        repo.legacy_maintenance_clusters()
            .await
            .unwrap()
            .iter()
            .any(|row| row.id.as_ref() == Some(&RecordId::parse_simple(&id).unwrap()))
    );
    f.mark_cluster_owned(&id).await;
    let members = repo.get_members(&id).await.unwrap();
    let before = f.observed().await;
    let result = repo
        .execute_split(&id, &members, &[0], &[1], &[1.0, 0.0], &[1.0, 0.0])
        .await;
    assert!(
        result.is_err(),
        "an earlier selection cannot authorize a now-owned source"
    );
    assert_eq!(f.observed().await, before);
}

#[tokio::test]
async fn legacy_maintenance_refuses_missing_or_unidentified_inputs_without_effects() {
    for scenario in [
        "missing-source",
        "missing-keep",
        "missing-remove",
        "missing-fact",
        "unidentified",
        "wrong-table",
    ] {
        let f = GateFixture::new().await;
        let source = f.cluster("valid source", false).await;
        let other = f.cluster("valid other", false).await;
        let repo = ClusterRepo::new(f.db.inner());
        let mut members = repo.get_members(&source).await.unwrap();
        let wrong_table = members[0].id.as_ref().unwrap().to_sql();
        if scenario == "missing-fact" {
            members[0].id = Some(RecordId::new("fact", "missing-input"));
        }
        if scenario == "unidentified" {
            members[0].id = None;
        }
        let before = f.observed().await;
        let result = match scenario {
            "missing-keep" => {
                repo.execute_merge("cluster:missing-input", &source, &[1.0, 0.0])
                    .await
            }
            "missing-remove" => {
                repo.execute_merge(&source, "cluster:missing-input", &[1.0, 0.0])
                    .await
            }
            _ => {
                let selected = match scenario {
                    "missing-source" => "cluster:missing-input",
                    "wrong-table" => &wrong_table,
                    _ => &source,
                };
                repo.execute_split(selected, &members, &[0], &[1], &[1.0, 0.0], &[1.0, 0.0])
                    .await
                    .map(|_| ())
            }
        };
        assert!(
            result.is_err(),
            "{scenario}: unknown inputs must not authorize effects"
        );
        assert_eq!(f.observed().await, before);
        assert!(repo.get(&other).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn legacy_maintenance_rejects_each_partial_fact_ownership_marker() {
    for marker in ["account", "workspace", "scope", "project"] {
        let f = GateFixture::new().await;
        let source = f.cluster("partial owner", false).await;
        let repo = ClusterRepo::new(f.db.inner());
        let members = repo.get_members(&source).await.unwrap();
        f.db.inner().query("UPDATE $fact SET account = $account, workspace = $workspace, scope = $scope, project = $project;")
            .bind(("fact", members[0].id.clone().unwrap()))
            .bind(("account", (marker == "account").then(|| f.owner.account.as_record_id().clone())))
            .bind(("workspace", (marker == "workspace").then(|| f.owner.workspace.as_record_id().clone())))
            .bind(("scope", (marker == "scope").then_some("personal")))
            .bind(("project", (marker == "project").then(|| RecordId::new("project", "partial-marker"))))
            .await.unwrap().check().unwrap();
        let before = f.observed().await;
        assert!(
            repo.execute_split(&source, &members, &[0], &[1], &[1.0, 0.0], &[1.0, 0.0])
                .await
                .is_err(),
            "{marker}"
        );
        assert_eq!(f.observed().await, before);
    }
}

#[tokio::test]
async fn legacy_maintenance_rejects_hidden_owned_merge_population_on_either_side() {
    for deleted in [false, true] {
        for mixed_kept in [false, true] {
            let f = GateFixture::new().await;
            let owned = f.cluster("outside owned", true).await;
            let mixed = f.cluster("mixed hidden", false).await;
            let other = f.cluster("eligible other", false).await;
            let repo = ClusterRepo::new(f.db.inner());
            let hidden = repo
                .get_members(&owned)
                .await
                .unwrap()
                .remove(0)
                .id
                .unwrap();
            let sql = if deleted {
                "UPDATE $fact SET deleted = true;"
            } else {
                "UPDATE $fact SET quarantined_at = d'2020-01-01T00:00:00Z';"
            };
            f.db.inner()
                .query(sql)
                .bind(("fact", hidden.clone()))
                .await
                .unwrap()
                .check()
                .unwrap();
            repo.add_member(&mixed, &hidden.to_sql()).await.unwrap();
            assert_eq!(
                repo.get_members(&mixed).await.unwrap().len(),
                2,
                "the owned retained member is hidden from live reads"
            );
            let before = f.observed().await;
            let (keep, remove) = if mixed_kept {
                (&mixed, &other)
            } else {
                (&other, &mixed)
            };
            assert!(repo.execute_merge(keep, remove, &[1.0, 0.0]).await.is_err());
            assert_eq!(f.observed().await, before);
        }
    }
}

#[tokio::test]
async fn legacy_maintenance_keeps_real_unowned_split_and_merge_usable() {
    let f = GateFixture::new().await;
    let _outside_owned = f.cluster("outside owned control", true).await;
    let source = f.cluster("usable legacy", false).await;
    let repo = ClusterRepo::new(f.db.inner());
    let members = repo.get_members(&source).await.unwrap();
    let (a, b) = repo
        .execute_split(&source, &members, &[0], &[1], &[1.0, 0.0], &[1.0, 0.0])
        .await
        .unwrap();
    assert!(repo.get(&source).await.unwrap().is_none());
    assert_eq!(repo.get_member_ids(&a).await.unwrap().len(), 1);
    assert_eq!(repo.get_member_ids(&b).await.unwrap().len(), 1);
    repo.execute_merge(&a, &b, &[1.0, 0.0]).await.unwrap();
    assert!(repo.get(&b).await.unwrap().is_none());
    assert_eq!(repo.get_member_ids(&a).await.unwrap().len(), 2);
    let logs = repo.list_maintenance_logs(10, 0, None).await.unwrap();
    assert!(logs.iter().any(|row| row.action == "split"));
    assert!(logs.iter().any(|row| row.action == "merge"));
}
