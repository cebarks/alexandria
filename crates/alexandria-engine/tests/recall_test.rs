use alexandria_engine::clusters::ClusterInfo;
use alexandria_engine::recall::{
    ClusterWithMembers, FactSummary, RecallHandle, broad_recall, focused_recall,
};
use alexandria_storage::models::ClusterVersion;

/// A cluster with its members, stamped with the version a handle would carry.
fn cluster(id: &str, centroid: Vec<f32>, members: Vec<FactSummary>) -> ClusterWithMembers {
    let members_len = members.len();
    ClusterWithMembers {
        info: ClusterInfo {
            id: id.into(),
            centroid: centroid.clone(),
            member_count: members_len,
        },
        version: ClusterVersion {
            membership_revision: Some(0),
            members: members_len,
        },
        members,
    }
}

fn fact(id: &str, content: &str, embedding: Vec<f32>, heat: f64) -> FactSummary {
    FactSummary {
        id: id.into(),
        content: content.into(),
        embedding,
        heat,
    }
}

#[test]
fn test_recall_handle_roundtrip_carries_locator_and_version() {
    let handle = RecallHandle::new(
        "cluster:abc123",
        ClusterVersion {
            membership_revision: Some(7),
            members: 3,
        },
    );

    let decoded = RecallHandle::decode(&handle.encode().unwrap()).unwrap();

    assert_eq!(decoded.cluster_id, handle.cluster_id);
    assert_eq!(decoded.version.members, 3);
    assert_eq!(decoded.version.membership_revision, Some(7));
}

/// A handle minted before the version field existed still decodes, and claims nothing.
///
/// Deployed clients hold handles across an upgrade. Refusing them would be a public break for no
/// security gain — the handle was never authority — so an absent `version` decodes to the starting
/// value, which says "no guard seen, no members counted" rather than pretending to describe a state.
#[test]
fn a_handle_issued_before_the_version_field_still_decodes() {
    let legacy = base64url("{\"cluster_id\":\"cluster:old\",\"depth\":0,\"issued_at\":1}");
    let decoded = RecallHandle::decode(&legacy).expect("a pre-version handle must still decode");
    assert_eq!(decoded.cluster_id, "cluster:old");
    assert_eq!(decoded.version.membership_revision, None);
    assert_eq!(decoded.version.members, 0);
}

/// Every unreadable handle fails with one text, so the refusal is not a report of which layer gave up.
#[test]
fn unreadable_handles_all_refuse_with_the_same_text() {
    let mut messages = Vec::new();
    for case in ["", "   ", "not base64!!", "{\"nonsense\":true}"] {
        let error = RecallHandle::decode(case)
            .err()
            .unwrap_or_else(|| panic!("{case:?} decoded, which it must not"));
        messages.push(format!("{error}"));
    }
    let distinct: std::collections::BTreeSet<&str> = messages.iter().map(String::as_str).collect();
    assert_eq!(
        distinct.len(),
        1,
        "unreadable handles refused differently: {messages:?}"
    );
}

/// The version is what makes a replay a *re-read*: it moves with the cluster, not with the clock.
#[test]
fn a_handles_version_reports_the_cluster_moving_and_time_not_moving_it() {
    let issued = RecallHandle::new(
        "cluster:c1",
        ClusterVersion {
            membership_revision: Some(0),
            members: 1,
        },
    );
    assert!(
        !issued.snapshot_moved(&ClusterVersion {
            membership_revision: Some(0),
            members: 1,
        }),
        "an unchanged cluster must not be reported as a moved snapshot"
    );
    assert!(
        issued.snapshot_moved(&ClusterVersion {
            membership_revision: Some(0),
            members: 2,
        }),
        "a member added since issuance must be reported as a moved snapshot"
    );
    assert!(
        issued.snapshot_moved(&ClusterVersion {
            membership_revision: Some(1),
            members: 1,
        }),
        "the stored guard moving must be reported, whatever the count does"
    );
}

#[test]
fn test_broad_recall_returns_cluster_matches() {
    let clusters = vec![cluster(
        "c1",
        vec![1.0, 0.0, 0.0],
        vec![
            fact("f1", "OAuth tokens", vec![0.9, 0.1, 0.0], 5.0),
            fact("f2", "JWT signing", vec![0.8, 0.2, 0.0], 3.0),
        ],
    )];

    let query = vec![0.95, 0.05, 0.0]; // auth-related
    let result = broad_recall(&query, &clusters, 5, 0.1);

    assert!(!result.clusters.is_empty());
    assert!(result.clusters[0].scope_handle.is_some());
    assert!(result.clusters[0].representative_memories.len() <= 3);
}

/// The handle broad recall issues is the cluster's locator *and* its version.
///
/// This is the engine half of the handle contract: a client that echoes back a handle gives the server
/// something it can check the cluster against, and the only way that can work is if the number was
/// stamped from the state the pass actually read.
#[test]
fn broad_recall_stamps_the_issue_time_version_into_the_handle() {
    let mut c1 = cluster(
        "c1",
        vec![1.0, 0.0],
        vec![fact("f1", "OAuth tokens", vec![0.9, 0.1], 5.0)],
    );
    c1.version = ClusterVersion {
        membership_revision: Some(4),
        members: 1,
    };

    let result = broad_recall(&[0.95, 0.05], &[c1], 5, 0.1);
    let handle = RecallHandle::decode(
        result.clusters[0]
            .scope_handle
            .as_deref()
            .expect("a matched cluster issues a handle"),
    )
    .unwrap();

    assert_eq!(handle.cluster_id, "c1");
    assert_eq!(handle.version.membership_revision, Some(4));
    assert_eq!(handle.version.members, 1);
}

#[test]
fn test_focused_recall_narrows_within_cluster() {
    let cluster_data = cluster(
        "c1",
        vec![1.0, 0.0],
        vec![
            fact("f1", "close match", vec![0.9, 0.1], 2.0),
            fact("f2", "medium match", vec![0.5, 0.5], 1.0),
            fact("f3", "far match", vec![0.1, 0.9], 0.5),
        ],
    );

    let scope = RecallHandle::new(
        "c1",
        ClusterVersion {
            membership_revision: Some(0),
            members: 3,
        },
    );
    let query = vec![1.0, 0.0];
    let result = focused_recall(&query, &scope, &cluster_data);

    assert_eq!(result.memories.len(), 3);
    assert!(
        !result.snapshot_moved,
        "the cluster is where the handle says it is, so the answer must not claim it moved"
    );
    // First result should be the closest match
    assert_eq!(result.memories[0].id, "f1");
}

/// A focused read answers with the cluster it was handed, and says the snapshot moved.
///
/// The rows come from `cluster_data` and nothing from the handle, which is the whole reason a handle can
/// be an unsigned string; `snapshot_moved` is the disclosure that goes with serving them.
#[test]
fn focused_recall_serves_current_rows_and_reports_a_moved_snapshot() {
    let mut cluster_data = cluster(
        "c1",
        vec![1.0, 0.0],
        vec![
            fact("f1", "close match", vec![0.9, 0.1], 2.0),
            fact("f2", "added later", vec![0.8, 0.2], 1.0),
        ],
    );
    cluster_data.version.members = 2;
    let stale = RecallHandle::new(
        "c1",
        ClusterVersion {
            membership_revision: Some(0),
            members: 1,
        },
    );

    let result = focused_recall(&[1.0, 0.0], &stale, &cluster_data);

    assert_eq!(
        result.memories.len(),
        2,
        "a stale handle must not narrow the answer to the rows it described"
    );
    assert!(
        result.memories.iter().any(|m| m.id == "f2"),
        "the member added after issuance was not served: {:?}",
        result.memories.iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    assert!(
        result.snapshot_moved,
        "the cluster moved and the result did not say so"
    );
}

#[test]
fn test_broad_recall_keeps_weak_but_real_matches_and_drops_noise() {
    // With all-MiniLM-L6-v2 a natural-language question against a stored
    // statement scores ~0.2 cosine; unrelated text scores ~0.0. The floor must
    // sit between those.
    let clusters = vec![
        cluster(
            "weak",
            vec![0.2, 0.98],
            vec![fact("weak-f", "weak", vec![0.2, 0.98], 1.0)],
        ),
        cluster(
            "noise",
            vec![0.0, 1.0],
            vec![fact("noise-f", "noise", vec![0.0, 1.0], 1.0)],
        ),
    ];

    let result = broad_recall(&[1.0, 0.0], &clusters, 5, 0.1);

    let ids: Vec<_> = result
        .clusters
        .iter()
        .map(|c| c.cluster_id.as_str())
        .collect();
    assert_eq!(ids, vec!["weak"]);
}

#[test]
fn test_broad_recall_honors_min_similarity() {
    // "weak" has centroid_sim ~0.2 against the query: above 0.1, below 0.5.
    let clusters = vec![
        cluster(
            "strong",
            vec![0.9, 0.1],
            vec![fact("strong-f", "strong", vec![0.9, 0.1], 1.0)],
        ),
        cluster(
            "weak",
            vec![0.2, 0.98],
            vec![fact("weak-f", "weak", vec![0.2, 0.98], 1.0)],
        ),
    ];

    let ids = |floor: f32| -> Vec<String> {
        broad_recall(&[1.0, 0.0], &clusters, 5, floor)
            .clusters
            .iter()
            .map(|c| c.cluster_id.clone())
            .collect()
    };
    assert_eq!(ids(0.1), vec!["strong", "weak"]);
    assert_eq!(ids(0.5), vec!["strong"]);
}

/// base64url-no-pad of a literal, for the legacy-handle case above.
///
/// Written here rather than by depending on the `base64` crate because the case is about what an *old
/// encoder* produced: a payload the current type never writes and the current decoder must still read.
fn base64url(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut group = [0u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let bits = ((group[0] as u32) << 16) | ((group[1] as u32) << 8) | group[2] as u32;
        for index in 0..(chunk.len() + 1) {
            out.push(ALPHABET[((bits >> (18 - 6 * index as u32)) & 0b11_1111) as usize] as char);
        }
    }
    out
}
