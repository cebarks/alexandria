//! Task 2 contracts: opaque typed record IDs, untrusted scope data, and `StorageError`.
//!
//! An integration test is the external-crate view — the MCP crate, engine and binary see exactly
//! what is exported here — so everything asserted below is a public contract. The compile-time
//! half of the contract (an authorized context cannot be built or deserialised outside storage,
//! while untrusted scope data can) is pinned by the `compile_fail` doctests in `src/access.rs`,
//! which `cargo test --locked -p alexandria-storage --doc` runs.

use std::error::Error;
use std::str::FromStr;

use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{
    AccountId, CredentialId, FactId, MemoryScope, OperationId, Partition, ProjectId, ReadSelection,
    ScopeKey, Version, WorkspaceId,
};
use surrealdb::types::{RecordId, RecordIdKey, Uuid};

/// The tables Task 2 hands out a typed ID for. Each parser must accept exactly one of them.
const ID_TABLES: &[&str] = &["account", "workspace", "project", "fact", "credential"];

/// Runs a typed-ID parser against a record from each of the four tables it does not own.
fn assert_rejects_foreign_tables(
    label: &str,
    own_table: &str,
    parse: fn(RecordId) -> Result<(), StorageError>,
) {
    for table in ID_TABLES {
        if *table == own_table {
            continue;
        }
        let foreign = RecordId::new(*table, "opaque-key");
        assert!(
            parse(foreign).is_err(),
            "{label} accepted a record from table `{table}`"
        );
    }
}

#[test]
fn opaque_identity_types_reject_wrong_table() {
    let project_record_id = RecordId::new("project", "p_1");
    assert!(AccountId::try_from(project_record_id).is_err());

    assert_rejects_foreign_tables("AccountId", "account", |id| {
        AccountId::try_from(id).map(|_| ())
    });
    assert_rejects_foreign_tables("WorkspaceId", "workspace", |id| {
        WorkspaceId::try_from(id).map(|_| ())
    });
    assert_rejects_foreign_tables("ProjectId", "project", |id| {
        ProjectId::try_from(id).map(|_| ())
    });
    assert_rejects_foreign_tables("FactId", "fact", |id| FactId::try_from(id).map(|_| ()));
    assert_rejects_foreign_tables("CredentialId", "credential", |id| {
        CredentialId::try_from(id).map(|_| ())
    });
}

#[test]
fn typed_ids_accept_their_own_table() {
    let account = AccountId::try_from(RecordId::new("account", "a_1")).unwrap();
    let workspace = WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap();
    let project = ProjectId::try_from(RecordId::new("project", "p_1")).unwrap();
    let fact = FactId::try_from(RecordId::new("fact", "f_1")).unwrap();
    let credential = CredentialId::try_from(RecordId::new("credential", "c_1")).unwrap();

    for (label, id) in [
        ("account", account.as_record_id()),
        ("workspace", workspace.as_record_id()),
        ("project", project.as_record_id()),
        ("fact", fact.as_record_id()),
        ("credential", credential.as_record_id()),
    ] {
        assert_eq!(id.table.as_str(), label, "{label} id kept the wrong table");
    }
}

/// The table check compares the parsed `RecordId`'s own table field. If it ever went through a SQL
/// or `table:key` string form, a table name that only looks acceptable when interpolated
/// (`account:1`, a padded name) would be accepted — and an awkward but genuine key would be the
/// thing that breaks.
#[test]
fn table_validation_reads_the_parsed_table_not_a_string_form() {
    for lookalike in [
        "account:1",
        "account`",
        "`account`",
        " account",
        "accounts",
        "Account",
    ] {
        assert!(
            AccountId::try_from(RecordId::new(lookalike, "opaque-key")).is_err(),
            "AccountId accepted table `{lookalike}`"
        );
    }

    let awkward_key = RecordId::new("account", "1:2`3 with spaces");
    let accepted = AccountId::try_from(awkward_key.clone()).unwrap();
    assert_eq!(accepted.as_record_id(), &awkward_key);
}

#[test]
fn numeric_record_id_keys_survive_the_typed_id_round_trip() {
    let original = RecordId::new("account", 239i64);
    let typed = AccountId::try_from(original.clone()).unwrap();

    assert_eq!(typed.as_record_id(), &original);
    assert!(
        matches!(typed.as_record_id().key, RecordIdKey::Number(239)),
        "numeric key came back as {:?}",
        typed.as_record_id().key
    );
}

#[test]
fn quoted_opaque_record_id_keys_survive_the_typed_id_round_trip() {
    for opaque in ["needs escaping", "he said `hi`", "a:b", "", "r#c", "ᚠᚢᚦ"] {
        let original = RecordId::new("project", opaque);
        let typed = ProjectId::try_from(original.clone()).unwrap();

        assert_eq!(
            typed.as_record_id(),
            &original,
            "key `{opaque}` was rewritten"
        );
        assert_eq!(typed.as_record_id().key, RecordIdKey::String(opaque.into()));
    }
}

#[test]
fn uuid_record_id_keys_survive_the_typed_id_round_trip() {
    let original = RecordId::new(
        "fact",
        Uuid::from_str("3f2504e0-4f89-41d3-9a0c-0305e82c3301").unwrap(),
    );
    let typed = FactId::try_from(original.clone()).unwrap();

    assert_eq!(typed.as_record_id(), &original);
    assert!(matches!(typed.as_record_id().key, RecordIdKey::Uuid(_)));
}

/// The wrong-table message has to be actionable without becoming a data leak: the rejected record
/// id is the caller's guess at somebody else's private row, and Task 9 renders these errors to
/// clients.
#[test]
fn wrong_table_error_names_the_expected_table_and_omits_the_rejected_record() {
    let rejected = RecordId::new("fact", "the-believed-password");
    let error = AccountId::try_from(rejected).unwrap_err();
    let message = error.to_string();

    assert!(
        message.contains("account"),
        "message must name the table the caller needed: {message}"
    );
    assert!(
        !message.contains("the-believed-password"),
        "message leaked the rejected key: {message}"
    );
    assert!(
        !message.contains("fact"),
        "message leaked the rejected table: {message}"
    );
}

#[test]
fn memory_scope_holds_only_a_validated_project_id() {
    let project = ProjectId::try_from(RecordId::new("project", "p_1")).unwrap();
    let scope = MemoryScope::Project(project.clone());

    let MemoryScope::Project(resolved) = &scope else {
        panic!("MemoryScope::Project lost its project id: {scope:?}");
    };
    assert_eq!(resolved.as_record_id().table.as_str(), "project");
    assert_ne!(scope, MemoryScope::Personal);
    assert_ne!(scope, MemoryScope::Unassigned);
}

/// `ScopeKey`, `ReadSelection` and `Partition` are the request-side shapes the MCP layer hands to
/// storage. They stay ordinary data: constructible, comparable, and carrying no authority, so
/// Task 3's resolution step — not possession of one of these — decides what may be read.
#[test]
fn untrusted_scope_data_is_plain_data_and_never_authority() {
    let workspace = WorkspaceId::try_from(RecordId::new("workspace", "w_1")).unwrap();
    let project = ProjectId::try_from(RecordId::new("project", "p_1")).unwrap();

    let partition = Partition {
        workspace: workspace.clone(),
        scope: MemoryScope::Personal,
    };
    assert_eq!(
        partition,
        Partition {
            workspace,
            scope: MemoryScope::Personal
        }
    );

    // Two accounts' projects called "alpha" are the same untrusted key and different scopes.
    assert_eq!(
        ScopeKey::Project("alpha".into()),
        ScopeKey::Project("alpha".into()),
        "the untrusted key is text, so equal names must compare equal"
    );
    assert_eq!(ScopeKey::Personal, ScopeKey::Personal);
    assert_eq!(ScopeKey::Unassigned, ScopeKey::Unassigned);
    assert_ne!(ScopeKey::Personal, ScopeKey::Unassigned);

    let ReadSelection::Auto(Some(resolved)) = &ReadSelection::Auto(Some(project.clone())) else {
        panic!("ReadSelection::Auto dropped its project selector");
    };
    assert_eq!(resolved, &project);
    assert_ne!(
        ReadSelection::Auto(None),
        ReadSelection::Auto(Some(project))
    );
    assert_ne!(ReadSelection::Assigned, ReadSelection::UnassignedReview);
    assert_ne!(
        ReadSelection::One(MemoryScope::Personal),
        ReadSelection::Assigned
    );
}

/// Every planned variant exists, renders a message, and renders a message no other variant
/// renders. Task 4 maps each one to a response, so two variants sharing a rendering would silently
/// merge distinct outcomes (semantic `Stale` versus a transient derived-bookkeeping conflict) into
/// one outward answer.
#[test]
fn storage_error_variants_render_distinct_messages() {
    let variants = [
        StorageError::NotFound,
        StorageError::InvalidInput("expected a record of table `account`".into()),
        StorageError::Stale,
        StorageError::RetryableBookkeepingConflict,
        StorageError::Quota,
        StorageError::Unavailable,
        StorageError::Internal(anyhow::anyhow!("cause")),
    ];

    for (i, first) in variants.iter().enumerate() {
        let message = first.to_string();
        assert!(!message.trim().is_empty(), "variant {i:?} renders nothing");
        for (j, second) in variants.iter().enumerate() {
            if i == j {
                continue;
            }
            assert_ne!(
                message,
                second.to_string(),
                "variants {i:?} and {j:?} render alike"
            );
        }
    }
}

/// `Internal` keeps the underlying failure for the log while its rendered message stays generic,
/// so a SQL driver detail can reach a trace but never a client body.
#[test]
fn internal_storage_error_keeps_the_cause_for_logs_but_not_for_rendering() {
    let error = StorageError::Internal(anyhow::anyhow!("engine rejected `SELECT value FROM x`"));

    assert!(
        Error::source(&error).is_some(),
        "the cause must stay reachable for tracing"
    );
    let message = error.to_string();
    assert!(
        !message.contains("SELECT"),
        "rendered message leaked the cause: {message}"
    );
    assert!(
        !message.contains("value"),
        "rendered message leaked the cause: {message}"
    );
}

/// Existing repo boundaries still return `anyhow::Result`; the typed error must cross that
/// boundary as itself rather than as a string that later code has to classify.
#[test]
fn storage_error_crosses_the_existing_anyhow_boundary_unclassified() {
    let boxed: anyhow::Error = StorageError::Unavailable.into();
    assert!(matches!(
        boxed.downcast_ref::<StorageError>(),
        Some(StorageError::Unavailable)
    ));

    fn assert_send_sync_static<E: Error + Send + Sync + 'static>() {}
    assert_send_sync_static::<StorageError>();
}

#[test]
fn version_orders_for_compare_and_swap_guards() {
    assert!(Version::new(3) < Version::new(4));
    assert_eq!(Version::new(7), Version::new(7));
    assert_eq!(Version::new(7).get(), 7);
    assert_eq!(Version::ZERO.get(), 0);
}

#[test]
fn operation_id_keeps_the_supplied_key_verbatim() {
    let id = OperationId::new("run-cli-1760000000-4242");
    assert_eq!(id.as_str(), "run-cli-1760000000-4242");
    assert_eq!(id, OperationId::new("run-cli-1760000000-4242"));
    assert_ne!(id, OperationId::new("run-cli-1760000000-9999"));
}
