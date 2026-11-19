//! RED contract tests for the owner refinement UI. These assertions intentionally describe the
//! complete owner-visible contract; Task 19's queue/attempt service and the subsequent UI slice
//! will make the behavioral tests pass. No external provider or live corpus is used here.

const QUEUE: &str = include_str!("../templates/refinement_queue.html");
const DETAIL: &str = include_str!("../templates/refinement_detail.html");
const SETTINGS: &str = include_str!("../templates/refinement_settings.html");

#[test]
fn queue_discloses_unassigned_and_review_ordering() {
    for text in [
        "Unassigned",
        "quarantined",
        "pending quality work",
        "assess",
        "apply",
        "classify",
        "all selected records",
        "quota",
        "attempt",
        "select",
        "records",
    ] {
        assert!(
            QUEUE.contains(text),
            "queue is missing required disclosure {text:?}"
        );
    }
}

#[test]
fn refinement_detail_shows_private_application_history() {
    for text in [
        "Source",
        "Changes",
        "Rationale",
        "Uncertainty",
        "same model",
        "different model",
        "operation",
        "actor",
        "preimage",
        "postimage",
        "revert",
    ] {
        assert!(DETAIL.contains(text), "proposal detail is missing {text:?}");
    }
    assert!(
        DETAIL.contains("{{"),
        "detail must render escaped proposal fields through Askama"
    );
    assert!(
        DETAIL.contains("|urlencode"),
        "proposal IDs in links must be URL-encoded"
    );
    assert!(!DETAIL.contains("operator health"));
    assert!(!DETAIL.contains("another account"));
    assert!(!DETAIL.contains("|safe"));
}

#[test]
fn stale_moved_proposals_and_bulk_approval_are_explicitly_blocked() {
    for text in [
        "stale",
        "moved",
        "reload",
        "No bulk quality approval",
        "selected",
    ] {
        assert!(
            DETAIL.contains(text),
            "proposal detail is missing safety disclosure {text:?}"
        );
    }
}

#[test]
fn settings_make_consent_and_human_default_explicit() {
    for text in [
        "Human approval",
        "default",
        "Automatic secondary review",
        "same model",
        "changed model",
        "endpoint",
        "name=\"csrf\"",
        "consent_revision",
        "mode_revision",
        "changed model",
        "same model",
        "quota",
        "attempt",
    ] {
        assert!(SETTINGS.contains(text), "settings are missing {text:?}");
    }
    assert!(!SETTINGS.contains("|safe"));
}
