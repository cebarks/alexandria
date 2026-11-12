//! The error every request-facing service boundary returns.
//!
//! Task 4 defines this type because the credential paths are the first to need it: a caller that presented
//! a password, a token, a cookie or a one-use link has to be told *which kind of no* it heard, without
//! being told anything about the rows behind the answer. Tasks 6 and up move the remaining repository
//! boundaries onto the same classes, so the mapping below is the only place a storage failure becomes a
//! service failure.
//!
//! ## The mapping table
//!
//! | `StorageError` | `ServiceError` | What the caller may conclude |
//! |---|---|---|
//! | `NotFound` | `NotFound` | the id is absent **or not yours** — one answer, so it cannot be probed |
//! | `InvalidInput(text)` | `InvalidInput(text)` | the request shape is wrong; the text is storage's own, which never echoes a submitted secret |
//! | `Stale` | `Stale` | the authority moved underneath the caller: revoked, consumed, expired, superseded `auth_epoch`, suspended account |
//! | `RetryableBookkeepingConflict` | same | a transient race over derived bookkeeping; the same request may be retried |
//! | `Quota` | `Quota` | a budget was reached — the request was not malformed |
//! | `Unavailable` | `Unavailable` | the store declined for load, not for authority |
//! | `Internal` whose cause **is** [`alexandria_storage::error::OwnerWorkspaceLost`] | `StoreIntegrity` | private owner state is missing from the store: a backup-or-recovery decision for whoever runs the server |
//! | `Internal`, any other cause | `Internal("store")` | an unclassified engine failure |
//!
//! Classification is by variant, then by the `anyhow` cause's **type**. No arm of that mapping looks at a
//! rendered message, and none must: a driver that words a write conflict like an integrity failure would
//! otherwise be reclassified by a coincidence of English.
//!
//! ## Why `Internal` carries a static tag and not the cause
//!
//! `StorageError::Internal` keeps an `anyhow` chain because storage is where the detail is known, and `Debug`
//! on it is the log-only channel (see the Task 3 review's P2-8: `Debug` stays in the log, clients see
//! sanitized `Display`). The service boundary is the other side of that line — it is what a handler renders
//! into a body — so it deliberately does not carry the chain forward. An engine refusal frequently quotes
//! the value it refused, and the values bound by the paths in this crate include credential digests and
//! password hashes. Dropping the cause here is the sanitization, and the static tag keeps the one thing an
//! operator can act on: which boundary class of failure this was.

use alexandria_storage::error::StorageError;
use thiserror::Error;

/// A request refused, in terms the caller can act on and nothing else.
#[derive(Error)]
pub enum ServiceError {
    /// The named id is absent, or belongs to somebody else. Both answers are this one answer, byte for
    /// byte, which is what makes a private id unprobeable.
    #[error("not found")]
    NotFound,
    /// The request itself is malformed. The text comes from storage's own validators, which name a field
    /// and never quote the submitted value back.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The authority behind the request has moved: revoked, consumed, expired, minted under a superseded
    /// authentication epoch, or suspended. The caller's remedy is to authenticate again, not to retry.
    #[error("stale")]
    Stale,
    /// A transient conflict over derived bookkeeping. Distinct from [`ServiceError::Stale`] on purpose:
    /// retrying is correct here and wrong there.
    #[error("retryable bookkeeping conflict")]
    RetryableBookkeepingConflict,
    /// A budget was reached. Not a malformed request, and not lost authority.
    #[error("quota exceeded")]
    Quota,
    /// The store declined for load.
    #[error("unavailable")]
    Unavailable,
    /// Private owner state is missing from the store. Owner decision F6: this is `Internal` in storage,
    /// because the caller did nothing wrong and nothing about *which* owner is affected may be inferred,
    /// and it is distinguishable here because whoever operates the server has a backup-or-recovery decision
    /// to make and should not have to read a log to find out that they do. The message names no table, no
    /// id and no column.
    #[error(
        "store integrity failure: private owner state is missing from the store; \
         a backup or recovery decision is required"
    )]
    StoreIntegrity,
    /// An unclassified engine failure. The category is a static string chosen by the mapping below — the
    /// cause chain stays in storage, where a value quoted by a driver could otherwise be carried into a
    /// client-visible render.
    #[error("internal error")]
    Internal(&'static str),
}

impl ServiceError {
    /// The static category of an internal failure, for a log line or a metric label. `None` for every
    /// other class, which is what a caller that wants to count only genuine faults should ask.
    pub fn internal_category(&self) -> Option<&'static str> {
        match self {
            ServiceError::Internal(category) => Some(category),
            _ => None,
        }
    }
}

/// Hand-written so that a `Debug` render — the log-only path — cannot acquire a cause chain, a SQL fragment
/// or a quoted bound value by accident. `InvalidInput`'s text is the only carried payload, and storage's
/// validators write it without echoing a submitted secret or credential.
impl std::fmt::Debug for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::NotFound => f.write_str("NotFound"),
            ServiceError::InvalidInput(message) => f
                .debug_struct("InvalidInput")
                .field("message", message)
                .finish(),
            ServiceError::Stale => f.write_str("Stale"),
            ServiceError::RetryableBookkeepingConflict => {
                f.write_str("RetryableBookkeepingConflict")
            }
            ServiceError::Quota => f.write_str("Quota"),
            ServiceError::Unavailable => f.write_str("Unavailable"),
            ServiceError::StoreIntegrity => f.write_str("StoreIntegrity"),
            ServiceError::Internal(category) => {
                write!(f, "Internal({category:?})")
            }
        }
    }
}

impl From<StorageError> for ServiceError {
    fn from(error: StorageError) -> Self {
        // The typed question first, while the value is still borrowed: `is_owner_workspace_lost` asks
        // whether the `Internal` cause *is* the marker type, so this arm cannot drift into matching an
        // error string, and every other `Internal` falls through to the plain internal class below.
        if error.is_owner_workspace_lost() {
            return ServiceError::StoreIntegrity;
        }
        match error {
            StorageError::NotFound => ServiceError::NotFound,
            StorageError::InvalidInput(message) => ServiceError::InvalidInput(message),
            StorageError::Stale => ServiceError::Stale,
            StorageError::RetryableBookkeepingConflict => {
                ServiceError::RetryableBookkeepingConflict
            }
            StorageError::Quota => ServiceError::Quota,
            StorageError::Unavailable => ServiceError::Unavailable,
            StorageError::Internal(_) => ServiceError::Internal("store"),
        }
    }
}

impl From<anyhow::Error> for ServiceError {
    /// The legacy path's rule, stated once so no call site has to remember it: an unclassified `anyhow`
    /// failure is `Internal`, and nothing about it is classified by reading its text. A failure that
    /// already carries a `StorageError` class keeps that class — Task 6 relies on this when it converts the
    /// remaining `anyhow::Result` repository boundaries, and flattening an already-classified error would
    /// turn a `Stale` into a retryable-looking 500. The static `"store"` label on the unclassified arm is
    /// deliberately not derived from the error's content: this boundary stops carrying causes rather than
    /// starting to summarise them.
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<StorageError>() {
            Ok(classified) => ServiceError::from(classified),
            Err(_) => ServiceError::Internal("store"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant renders the same way through `Display`, `Debug` and the trait object, and none of them
    /// can carry SQL, a cause chain, a table name, a record id or credential material outward. That is the
    /// P2-8 half of this type: the body a client may be shown is the `Display`, so the *other* renders are
    /// checked against the same list.
    #[test]
    fn every_variant_renders_without_internals() {
        let variants = vec![
            ServiceError::from(StorageError::NotFound),
            ServiceError::from(StorageError::InvalidInput(
                "login must be between 3 and 64 characters".to_string(),
            )),
            ServiceError::from(StorageError::Stale),
            ServiceError::from(StorageError::RetryableBookkeepingConflict),
            ServiceError::from(StorageError::Quota),
            ServiceError::from(StorageError::Unavailable),
            ServiceError::from(StorageError::owner_workspace_lost()),
            ServiceError::from(StorageError::Internal(anyhow::anyhow!("driver said no"))),
        ];
        for variant in variants {
            for rendered in [
                variant.to_string(),
                format!("{variant:?}"),
                format!("{:?}", Box::new(variant) as Box<dyn std::error::Error>),
            ] {
                for forbidden in [
                    "SELECT",
                    "UPDATE",
                    "CREATE",
                    "DEFINE",
                    "credential",
                    "account:",
                    "workspace",
                    "$argon2id",
                    "secret_digest",
                    "password_hash",
                    "Caused by",
                ] {
                    assert!(
                        !rendered.contains(forbidden),
                        "a render carried {forbidden:?}: {rendered}"
                    );
                }
            }
        }
    }

    /// The mapping is by variant and typed cause: an `Internal` whose message *says* the lost-workspace
    /// words is still the plain internal class, and the marker type still becomes `StoreIntegrity`.
    #[test]
    fn mapping_classifies_by_variant_and_typed_cause() {
        assert!(matches!(
            ServiceError::from(StorageError::owner_workspace_lost()),
            ServiceError::StoreIntegrity
        ));
        let lookalike = StorageError::Internal(anyhow::anyhow!(
            "the private state an enrolled owner is entitled to is missing from the store"
        ));
        assert!(
            matches!(
                ServiceError::from(lookalike),
                ServiceError::Internal("store")
            ),
            "an internal failure was classified by matching its message"
        );
        // Every other class survives the trip, and nothing is flattened into `Internal`.
        for (input, expected) in [
            (StorageError::NotFound, "not found"),
            (StorageError::Stale, "stale"),
            (StorageError::Quota, "quota exceeded"),
            (StorageError::Unavailable, "unavailable"),
            (
                StorageError::RetryableBookkeepingConflict,
                "retryable bookkeeping conflict",
            ),
        ] {
            let kind = std::mem::discriminant(&input);
            let mapped = ServiceError::from(input);
            assert_eq!(mapped.to_string(), expected, "{kind:?} drifted");
            assert_eq!(mapped.internal_category(), None);
        }
    }

    /// An `anyhow` failure that already carries a `StorageError` keeps its class instead of becoming a
    /// generic internal error — the AGENTS.md rule, stated at the boundary the later tasks will use.
    #[test]
    fn an_anyhow_path_keeps_an_already_classified_error() {
        let classified = anyhow::Error::new(StorageError::Stale);
        assert_eq!(ServiceError::from(classified).to_string(), "stale");

        let wrapped = anyhow::Error::new(StorageError::NotFound)
            .context("looking up the presented credential");
        assert_eq!(ServiceError::from(wrapped).to_string(), "not found");

        let unclassified = anyhow::anyhow!("the connection dropped");
        let mapped = ServiceError::from(unclassified);
        assert_eq!(mapped.internal_category(), Some("store"));
        assert_eq!(mapped.to_string(), "internal error");
        // The category is metadata, not a message: nothing from the cause reaches the client render.
        assert!(!mapped.to_string().contains("dropped"));
    }

    /// `InvalidInput` is the one variant that carries text, and it is storage's own wording about the
    /// request — so the boundary keeps it, and the redaction tests above prove it stayed free of internals.
    #[test]
    fn invalid_input_text_passes_through_unchanged() {
        let mapped = ServiceError::from(StorageError::InvalidInput(
            "login must not be blank".to_string(),
        ));
        match mapped {
            ServiceError::InvalidInput(message) => assert_eq!(message, "login must not be blank"),
            other => panic!("expected an invalid-input refusal, got {other:?}"),
        }
    }
}
