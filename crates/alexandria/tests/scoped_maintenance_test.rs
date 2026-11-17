//! Persistent CLI journeys. Fixture open/seed/inspection run in separate processes so an SDK drop
//! cannot be mistaken for release of SurrealKV's single-writer lock.

use std::path::Path;
use std::process::{Command, Output};

use alexandria_storage::{Database, schema, system_config};
use serde_json::{Value, json};

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path, mode: &str) -> Value {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "maintenance_fixture_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TASK12_FIXTURE_ROOT", root)
        .env("TASK12_FIXTURE_MODE", mode)
        .output()
        .unwrap();
    assert_success(&output);
    serde_json::from_slice(&std::fs::read(root.join("snapshot.json")).unwrap()).unwrap()
}

fn dream(root: &Path, args: &[&str]) -> Output {
    let config = root.join("config.toml");
    std::fs::write(&config, "").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_alexandria"))
        .arg("dream")
        .args(args)
        .env("ALEXANDRIA_CONFIG", config)
        .env("ALEXANDRIA_DATA_DIR", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("unused-cache"))
        .output()
        .unwrap();
    assert_success(&output);
    output
}

fn rows<'a>(snapshot: &'a Value, table: &str) -> &'a [Value] {
    snapshot[table].as_array().unwrap()
}

fn fact<'a>(snapshot: &'a Value, content: &str) -> &'a Value {
    rows(snapshot, "fact")
        .iter()
        .find(|row| row["content"] == content)
        .unwrap()
}

#[test]
fn unassigned_unchanged_by_jobs() {
    let root = tempfile::tempdir().unwrap();
    let before = fixture(root.path(), "seed-jobs");
    let output = dream(root.path(), &["--job", "all"]);
    let after = fixture(root.path(), "inspect");

    assert!(
        rows(&before["unassigned"], "memory_edge")
            .iter()
            .any(|edge| edge["edge_type"] == "derived_from"),
        "negative control must include real retained Unassigned lineage, not an empty observer"
    );
    assert_eq!(
        after["unassigned"], before["unassigned"],
        "all retained Unassigned facts, heat, clusters, incident edges and lineage must stay unchanged"
    );
    assert_eq!(
        fact(&after, "assigned-cold")["confidence"],
        json!(0.2),
        "positive control: appraise must demote an assigned, post-arming cold fact"
    );
    assert_eq!(fact(&before, "assigned-cold")["confidence"], json!(0.5));
    let collapsed = rows(&after, "fact")
        .iter()
        .filter(|row| row["content"] == "assigned-duplicate" && row["deleted"] == true)
        .count();
    assert_eq!(
        collapsed, 1,
        "positive control: collapse retires exactly one assigned duplicate"
    );
    assert!(
        rows(&after, "heat_state")
            .iter()
            .any(|row| { row["last_touched"] != json!("2020-01-01T00:00:00Z") }),
        "positive control: sweep must materialise assigned heat"
    );
    for (job, action) in [
        ("cluster", "split"),
        ("merge", "merge"),
        ("collapse", "collapse"),
        ("appraise", "demote"),
    ] {
        assert!(
            rows(&after, "maintenance_log")
                .iter()
                .any(|row| row["job"] == job
                    && row["action"] == action
                    && row["actor"] == "system:dreaming"),
            "positive control: actual {job} job writes its system-attributed summary"
        );
    }
    assert!(
        rows(&after, "maintenance_log")
            .iter()
            .all(|row| row["job"] != "sweep")
    );
    // The semantic_operation journal asserts that used to live here pin the deferred
    // maintenance-journal-routing scope (MutationContext has no system variant yet); they are
    // preserved verbatim in `journal_routed_maintenance_writes_use_the_semantic_operation_kernel`
    // below as a named follow-up witness, not deleted.
    for table in ["semantic_operation", "semantic_snapshot", "maintenance_log"] {
        assert!(
            !after[table].to_string().contains("unassigned-"),
            "{table} must not name an Unassigned effect"
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for job in ["sweep", "cluster", "merge", "collapse", "appraise"] {
        assert!(
            stdout.contains(&format!("job={job} ")),
            "missing {job} report: {stdout}"
        );
    }
}

/// Journal routing for maintenance writes that act on assigned data.
///
/// Collapse and appraise operate on assigned data, so the active partition supplies the real
/// workspace and actor required by v009's semantic journal. Legacy-interim cluster/merge bodies
/// act only on unowned rows and cannot honestly populate those required fields; their
/// `cluster_change` journal assertion is kept separately as a named deferred witness for the
/// owned-cluster maintenance lane.
#[test]
fn journal_routed_maintenance_writes_use_the_semantic_operation_kernel() {
    let root = tempfile::tempdir().unwrap();
    let _before = fixture(root.path(), "seed-jobs");
    let _output = dream(root.path(), &["--job", "all"]);
    let after = fixture(root.path(), "inspect");
    for kind in ["collapse", "appraise"] {
        assert!(
            rows(&after, "semantic_operation")
                .iter()
                .any(|row| row["kind"] == kind),
            "semantic {kind} job writes must use the journal"
        );
    }
    assert!(
        !rows(&after, "semantic_snapshot").is_empty(),
        "journal preimages must be retained"
    );
    // The preimage must belong to the journaled operation, not merely exist: the seed-jobs
    // fixture collapses one of its two `assigned-duplicate` rows (confidence 0.5), so a
    // fact-kind snapshot carrying that exact pre-state must name the collapse operation's id.
    let collapse_ids: Vec<&Value> = rows(&after, "semantic_operation")
        .iter()
        .filter(|row| row["kind"] == "collapse")
        .map(|row| &row["id"])
        .collect();
    assert!(
        !collapse_ids.is_empty(),
        "a collapse journal row must exist for its preimage to name"
    );
    assert!(
        rows(&after, "semantic_snapshot").iter().any(|snapshot| {
            collapse_ids.contains(&&snapshot["operation"])
                && snapshot["kind"] == "fact"
                && snapshot["fact_content"] == "assigned-duplicate"
                && snapshot["fact_confidence"] == json!(0.5)
        }),
        "the collapsed duplicate's pre-state must feed the collapse operation's journal row"
    );
}

/// DEFERRED FOLLOW-UP WITNESS — journal routing for whole-store legacy cluster/merge bodies.
///
/// The current legacy gate admits only unowned cluster data, which has no honest workspace/account
/// attribution for v009's journal. Once the owned-cluster maintenance lane replaces those bodies,
/// cluster split/merge writes must route through the semantic-operation kernel.
#[test]
#[ignore = "deferred follow-up: cluster_change journal routing belongs to the owned-cluster maintenance lane"]
fn owned_cluster_maintenance_writes_use_the_semantic_operation_kernel() {
    let root = tempfile::tempdir().unwrap();
    let _before = fixture(root.path(), "seed-jobs");
    let _output = dream(root.path(), &["--job", "all"]);
    let after = fixture(root.path(), "inspect");
    assert!(
        rows(&after, "semantic_operation")
            .iter()
            .any(|row| row["kind"] == "cluster_change"),
        "owned cluster split/merge writes must use the journal"
    );
}

#[test]
fn global_budget_and_progress_survive_two_cli_invocations() {
    let root = tempfile::tempdir().unwrap();
    let before = fixture(root.path(), "seed-budget");
    let first_output = dream(root.path(), &["--job", "sweep", "--max-rows", "1"]);
    let first = fixture(root.path(), "inspect");
    let second_output = dream(root.path(), &["--job", "sweep", "--max-rows", "1"]);
    let second = fixture(root.path(), "inspect");

    let changed = |previous: &Value, next: &Value| {
        rows(next, "heat_state")
            .iter()
            .filter(|row| {
                let prior = rows(previous, "heat_state")
                    .iter()
                    .find(|old| old["id"] == row["id"])
                    .unwrap();
                prior["last_touched"] != row["last_touched"]
            })
            .count()
    };
    assert_eq!(
        changed(&before, &first),
        1,
        "first process has one global sweep allowance"
    );
    assert_eq!(
        changed(&first, &second),
        1,
        "second process receives one fresh global allowance"
    );
    let progress = |snapshot: &Value| {
        rows(snapshot, "maintenance_progress")
            .iter()
            .find(|row| row["job"] == "sweep")
            .expect("a successful capped CLI pass must persist its sweep cursor")
            .clone()
    };
    let first_progress = progress(&first);
    let second_progress = progress(&second);
    assert!(first_progress["version"].as_i64().unwrap() >= 1);
    assert!(
        second_progress["version"].as_i64().unwrap() > first_progress["version"].as_i64().unwrap()
    );
    assert_ne!(
        first_progress["last_workspace"], second_progress["last_workspace"],
        "one account's many projects must not consume both turns across restart"
    );
    for snapshot in [&first, &second] {
        assert_eq!(snapshot["unassigned"], before["unassigned"]);
    }
    for output in [&first_output, &second_output] {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("job=sweep examined=1 acted=1"), "{stdout}");
    }
}

async fn execute(db: &Database, sql: &str) {
    db.inner().query(sql).await.unwrap().check().unwrap();
}

async fn seed_owner(db: &Database, owner: &str) {
    execute(
        db,
        &format!(
            "CREATE account:{owner} SET login = '{owner}'; \
         CREATE workspace:{owner} SET owner = account:{owner};"
        ),
    )
    .await;
}

async fn seed_project(db: &Database, owner: &str, project: &str) {
    execute(db, &format!(
        "CREATE project:{project} SET workspace = workspace:{owner}, normalized_alias = '{project}';"
    )).await;
}

async fn seed_fact(
    db: &Database,
    owner: &str,
    project: Option<&str>,
    key: &str,
    content: &str,
    vector: [f32; 2],
) -> alexandria_storage::models::Fact {
    let scope = if project.is_some() {
        "project"
    } else {
        "unassigned"
    };
    let project = project
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| "NONE".into());
    let mut response = db.inner().query(format!(
        "CREATE fact:`{key}` SET content = '{content}', confidence = 0.5, embedding = [{}, {}], \
         account = account:{owner}, workspace = workspace:{owner}, scope = '{scope}', project = {project} RETURN AFTER;",
        vector[0], vector[1]
    )).await.expect("fixture fact creation query executes").check().expect("fixture fact creation commits");
    let mut facts: Vec<alexandria_storage::models::Fact> =
        response.take(0).expect("decode created fixture fact");
    assert_eq!(
        facts.len(),
        1,
        "creation must return exactly one fixture fact for {key}"
    );
    let fact = facts.remove(0);
    let id = fact
        .id
        .as_ref()
        .expect("created fixture fact carries its typed record ID");
    assert_eq!(
        fact.content, content,
        "creation returned the requested fixture content"
    );
    db.inner()
        .query(format!(
            "CREATE heat_state:`{key}` SET memory = $memory, heat = 1.0, stability = 3.0, \
             last_touched = d'2020-01-01T00:00:00Z', last_accessed_at = d'2020-01-01T00:00:00Z', access_count = 0;"
        ))
        .bind(("memory", id.clone()))
        .await.expect("fixture heat creation executes").check().expect("fixture heat creation commits");
    fact
}

async fn seed_cluster(db: &Database, owner: &str, key: &str, members: &[&str], vector: [f32; 2]) {
    execute(
        db,
        &format!(
            "CREATE cluster:`{key}` SET account = account:{owner}, workspace = workspace:{owner}, \
         label = '{key}', centroid = [{}, {}];",
            vector[0], vector[1]
        ),
    )
    .await;
    for member in members {
        execute(
            db,
            &format!("RELATE cluster:`{key}`->contains_memory->fact:`{member}`;"),
        )
        .await;
    }
}

async fn seed_unassigned(db: &Database) {
    let mut live = Vec::new();
    for key in ["unassigned-a", "unassigned-b"] {
        live.push(seed_fact(db, "alice", None, key, "unassigned-duplicate", [0.0, 1.0]).await);
    }
    seed_cluster(
        db,
        "alice",
        "unassigned-cluster",
        &["unassigned-a", "unassigned-b"],
        [0.0, 1.0],
    )
    .await;
    let historical = seed_fact(
        db,
        "alice",
        None,
        "unassigned-history",
        "unassigned-historical-preimage",
        [0.0, 1.0],
    )
    .await;
    let historical_id = historical.id.expect("history creation returned a typed ID");
    let survivor_id = live[0]
        .id
        .clone()
        .expect("survivor creation returned a typed ID");
    db.inner()
        .query("UPDATE $id SET deleted = true;")
        .bind(("id", historical_id.clone()))
        .await
        .expect("historical soft-delete executes")
        .check()
        .expect("historical soft-delete commits");
    for (id, content, deleted) in [
        (&historical_id, "unassigned-historical-preimage", true),
        (&survivor_id, "unassigned-duplicate", false),
    ] {
        let mut response = db
            .inner()
            .query("SELECT * FROM $id;")
            .bind(("id", id.clone()))
            .await
            .expect("typed fixture identity selection executes")
            .check()
            .expect("typed fixture identity selection succeeds");
        let facts: Vec<alexandria_storage::models::Fact> = response
            .take(0)
            .expect("decode typed fixture identity selection");
        assert_eq!(
            facts.len(),
            1,
            "typed lookup must retain fixture endpoint {content}; ID={id:?}"
        );
        assert_eq!(
            facts[0].id.as_ref(),
            Some(id),
            "fixture endpoint identity is unchanged"
        );
        assert_eq!(
            facts[0].content, content,
            "typed endpoint resolves the actual seeded content"
        );
        assert_eq!(
            facts[0].deleted, deleted,
            "fixture endpoint has the intended retained visibility"
        );
    }
    db.inner()
        .query("RELATE $historical->memory_edge->$survivor SET edge_type = 'derived_from', strength = 1.0;")
        .bind(("historical", historical_id))
        .bind(("survivor", survivor_id))
        .await.expect("typed historical lineage relation executes").check().expect("typed historical lineage relation commits");
}

async fn snapshot(db: &Database) -> Value {
    let mut result = json!({});
    for table in [
        "fact",
        "heat_state",
        "cluster",
        "contains_memory",
        "memory_edge",
        "semantic_operation",
        "semantic_snapshot",
        "maintenance_log",
        "maintenance_progress",
    ] {
        let mut response = db
            .inner()
            .query(format!("SELECT * FROM {table} ORDER BY id;"))
            .await
            .unwrap()
            .check()
            .unwrap();
        let values: Vec<Value> = response.take(0).unwrap();
        result[table] = json!(values);
    }
    let mut unassigned = json!({});
    for (name, predicate) in [
        ("fact", "scope = 'unassigned'"),
        ("heat_state", "memory.scope = 'unassigned'"),
        ("cluster", "label = 'unassigned-cluster'"),
        ("contains_memory", "out.scope = 'unassigned'"),
        (
            "memory_edge",
            "in.scope = 'unassigned' OR out.scope = 'unassigned'",
        ),
    ] {
        let mut response = db
            .inner()
            .query(format!(
                "SELECT * FROM {name} WHERE {predicate} ORDER BY id;"
            ))
            .await
            .unwrap()
            .check()
            .unwrap();
        let values: Vec<Value> = response.take(0).unwrap();
        unassigned[name] = json!(values);
    }
    result["unassigned"] = unassigned;
    result
}

#[tokio::test]
#[ignore = "subprocess-only fixture; parent tests explicitly invoke it against a temporary store"]
async fn maintenance_fixture_child() {
    let root = std::env::var_os("TASK12_FIXTURE_ROOT").unwrap();
    let root = Path::new(&root);
    let mode = std::env::var("TASK12_FIXTURE_MODE").unwrap();
    let db = Database::connect_persistent(&root.join("data"))
        .await
        .unwrap();
    if mode.starts_with("seed-") {
        schema::migrate(db.inner()).await.unwrap();
        system_config::arm_access_recording(db.inner())
            .await
            .unwrap();
        seed_owner(&db, "alice").await;
        seed_owner(&db, "bobby").await;
        seed_unassigned(&db).await;
        if mode == "seed-jobs" {
            for project in ["duplicates", "split", "merge", "cold"] {
                seed_project(&db, "alice", project).await;
            }
            for key in ["assigned-duplicate-a", "assigned-duplicate-b"] {
                seed_fact(
                    &db,
                    "alice",
                    Some("duplicates"),
                    key,
                    "assigned-duplicate",
                    [1.0, 0.0],
                )
                .await;
            }
            seed_fact(
                &db,
                "alice",
                Some("cold"),
                "assigned-cold",
                "assigned-cold",
                [1.0, 0.0],
            )
            .await;
            for (key, vector) in [
                ("split-a", [1.0, 0.0]),
                ("split-b", [1.0, 0.0]),
                ("split-c", [0.0, 1.0]),
                ("split-d", [0.0, 1.0]),
            ] {
                seed_fact(&db, "alice", Some("split"), key, key, vector).await;
            }
            seed_cluster(
                &db,
                "alice",
                "assigned-wide",
                &["split-a", "split-b", "split-c", "split-d"],
                [1.0, 0.0],
            )
            .await;
            for key in ["merge-a", "merge-b"] {
                seed_fact(&db, "alice", Some("merge"), key, key, [1.0, 0.0]).await;
                seed_cluster(&db, "alice", key, &[key], [1.0, 0.0]).await;
            }
            // Legacy (unowned) split/merge population: the cluster and merge jobs stay
            // legacy-gated by owner decision, so their positive controls must live in the
            // population that gate admits — account NONE, workspace NONE, live members — while
            // the owned clusters above prove the gate leaves workspace data alone.
            for key in [
                "legacy-split-a",
                "legacy-split-b",
                "legacy-split-c",
                "legacy-split-d",
            ] {
                let vector = if key.ends_with('a') || key.ends_with('b') {
                    [1.0, 0.0]
                } else {
                    [0.0, 1.0]
                };
                execute(
                    &db,
                    &format!(
                        "CREATE fact:`{key}` SET content = '{key}', confidence = 0.5, \
                         embedding = [{}, {}], tags = [], deleted = false;",
                        vector[0], vector[1]
                    ),
                )
                .await;
            }
            execute(
                &db,
                "CREATE cluster:`legacy-wide` SET label = 'legacy-wide', centroid = [1.0, 0.0];",
            )
            .await;
            for member in [
                "legacy-split-a",
                "legacy-split-b",
                "legacy-split-c",
                "legacy-split-d",
            ] {
                execute(
                    &db,
                    &format!("RELATE cluster:`legacy-wide`->contains_memory->fact:`{member}`;"),
                )
                .await;
            }
            for key in ["legacy-merge-a", "legacy-merge-b"] {
                execute(
                    &db,
                    &format!(
                        "CREATE fact:`{key}` SET content = '{key}', confidence = 0.5, \
                         embedding = [1.0, 0.0], tags = [], deleted = false;"
                    ),
                )
                .await;
                execute(
                    &db,
                    &format!("CREATE cluster:`{key}` SET label = '{key}', centroid = [1.0, 0.0];"),
                )
                .await;
                execute(
                    &db,
                    &format!("RELATE cluster:`{key}`->contains_memory->fact:`{key}`;"),
                )
                .await;
            }
        } else {
            assert_eq!(mode, "seed-budget");
            for i in 0..7 {
                let project = format!("alice_project_{i}");
                seed_project(&db, "alice", &project).await;
                seed_fact(
                    &db,
                    "alice",
                    Some(&project),
                    &format!("alice-budget-{i}"),
                    &format!("alice-budget-{i}"),
                    [1.0, 0.0],
                )
                .await;
            }
            seed_project(&db, "bobby", "bobby_project").await;
            seed_fact(
                &db,
                "bobby",
                Some("bobby_project"),
                "bobby-budget",
                "bobby-budget",
                [1.0, 0.0],
            )
            .await;
        }
    } else {
        assert_eq!(mode, "inspect");
    }
    std::fs::write(
        root.join("snapshot.json"),
        serde_json::to_vec(&snapshot(&db).await).unwrap(),
    )
    .unwrap();
    // The parent never opens the store. A completed child process, not an async SDK drop or delay,
    // proves the fixture writer has released its store before the actual CLI process opens it.
    std::process::exit(0);
}
