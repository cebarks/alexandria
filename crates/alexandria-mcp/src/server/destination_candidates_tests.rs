use super::*;

fn cluster(id: Option<RecordId>, centroid: &[f32]) -> Cluster {
    Cluster {
        id,
        label: None,
        centroid: centroid.to_vec(),
        depth: 0,
        created_at: None,
    }
}

#[test]
fn typed_candidate_catalog_ranks_opaque_labels_without_reparsing_identity() {
    let preferred = RecordId::new("cluster", "`literal`");
    let catalog = DestinationCandidates::from_clusters(vec![
        (cluster(Some(preferred.clone()), &[1.0, 0.0]), 2),
        (
            cluster(Some(RecordId::new("cluster", "literal")), &[0.0, 1.0]),
            3,
        ),
    ])
    .unwrap();
    assert_eq!(
        catalog
            .infos
            .iter()
            .map(|info| info.id.as_str())
            .collect::<Vec<_>>(),
        ["0", "1"]
    );
    assert_eq!(catalog.infos[0].member_count, 2);
    assert_eq!(catalog.infos[1].member_count, 3);
    assert_eq!(catalog.choose(&[1.0, 0.0], 0.75).unwrap(), Some(&preferred));
    assert!(catalog.choose(&[-1.0, 0.0], 0.75).unwrap().is_none());
}

#[test]
fn typed_candidate_catalog_refuses_a_candidate_without_identity() {
    assert!(matches!(
        DestinationCandidates::from_clusters(vec![(cluster(None, &[1.0, 0.0]), 1)]),
        Err(StorageError::InvalidInput(_))
    ));
}

#[test]
fn typed_candidate_catalog_refuses_a_ranked_token_without_an_original_handle() {
    let mut catalog = DestinationCandidates::from_clusters(vec![(
        cluster(Some(RecordId::new("cluster", "known")), &[0.0, 1.0]),
        1,
    )])
    .unwrap();
    // Simulate an unmatched ranking entry without introducing a production injection seam.
    catalog.infos.push(ClusterInfo {
        id: "uncaptured-winner".to_string(),
        centroid: vec![1.0, 0.0],
        member_count: 1,
    });
    assert!(matches!(
        catalog.choose(&[1.0, 0.0], 0.75),
        Err(StorageError::InvalidInput(_))
    ));
}
