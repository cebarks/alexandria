//! Lossless PROJECT wire handles. Display labels are not selectors or authority.
//!
//! Canonical record text preserves numeric/string/quoted keys. Parsing is local and synchronous;
//! callers must still resolve the resulting typed project under their existing owner context.
//! This module also hosts the strict full-input SurrealQL value parser and, on top of it,
//! [`parse_record_id`] — the general record-id entry point every wire-facing boundary shares.

use crate::error::StorageError;
use crate::models::ProjectId;
use surrealdb::types::{RecordId, ToSql, Value};

/// Parse one SurrealQL value and require the whole input to be consumed.
///
/// The SDK's `surrealdb::parse::value` and core's own `syn::value` both parse a leading value and
/// return it without checking for trailing text, so `project:7; project:8` would silently parse as
/// `project:7`. The callback form below is the same native parser with the EOF assertion the
/// wrapper omits — the mechanism core itself uses for `duration` and its `*_for_definition`
/// entry points — so equivalent alternate literal forms (e.g. `(project:7)`) stay accepted while
/// any non-whitespace tail is refused. No delimiter scanning, no canonical-text comparison.
fn parse_full_value(text: &str) -> Result<Value, StorageError> {
    surrealdb_core::syn::parse_with(text.as_bytes(), async |parser, stk| {
        let value = parser.parse_value(stk).await?;
        parser.assert_finished()?;
        Ok(value)
    })
    .map_err(|_| StorageError::InvalidInput("expected one complete SurrealQL value".into()))
}

/// Serialize a known project record (from a ProjectId or a schema-typed project field).
/// This is a wire representation, not an ownership check or the general record display formatter.
pub fn format_project_handle(project: &RecordId) -> String {
    project.to_sql()
}

/// Accept exactly a project record literal, never a range, expression, name, or another table.
pub fn parse_project_handle(text: &str) -> Result<ProjectId, StorageError> {
    match parse_full_value(text) {
        Ok(Value::RecordId(id)) => ProjectId::try_from(id),
        _ => Err(StorageError::InvalidInput(
            "expected a literal owned project ID".into(),
        )),
    }
}

/// The explicit resolver also accepts aliases. Recognized record selectors still require the
/// project table; malformed project-prefixed text and ranges must not become newly created aliases.
pub fn parse_project_selector(text: &str) -> Result<Option<ProjectId>, StorageError> {
    match parse_full_value(text) {
        Ok(Value::RecordId(id)) => ProjectId::try_from(id).map(Some),
        Ok(Value::Range(_)) => Err(StorageError::InvalidInput(
            "expected a project point, not a range".into(),
        )),
        _ if text.starts_with("project:") => Err(StorageError::InvalidInput(
            "malformed project selector".into(),
        )),
        _ => Ok(None),
    }
}

/// Parse exactly one record id, and nothing else.
///
/// `RecordId::parse_simple` splits at the first colon and keeps unwrapped fragments as strings, so
/// `fact:x; junk` succeeds as table `fact` with the invented key `"x; junk"`, and `fact:7` never
/// names the numeric-keyed record 7 — the trailing-input defect and the numeric-key class, on
/// every path that parses a caller-supplied id. This is the strict counterpart: the whole input
/// must be one record point literal (numeric, string, quoted and uuid keys all parse as
/// themselves), and ranges, non-record values and trailing text are refused. Callers keep their
/// own failure mapping; only the acceptance of garbage changes.
pub fn parse_record_id(text: &str) -> Result<RecordId, StorageError> {
    match parse_full_value(text) {
        Ok(Value::RecordId(id)) => Ok(id),
        _ => Err(StorageError::InvalidInput(
            "expected a single record id".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_handle_preserves_typed_and_quoted_key_identity() {
        const UUID_TEXT: &str = "123e4567-e89b-42d3-a456-426614174000";
        let uuid = UUID_TEXT.parse::<surrealdb::types::Uuid>().unwrap();
        for id in [
            RecordId::new("project", 7_i64),
            RecordId::new("project", "7"),
            RecordId::new("project", "`ticktwin`"),
            RecordId::new("project", "ticktwin"),
            RecordId::new("project", uuid),
            RecordId::new("project", UUID_TEXT),
            RecordId::new("project", "quote'colon:slash/space ü"),
            RecordId::new("project", "inside;semicolon"),
        ] {
            assert_eq!(
                parse_project_handle(&format_project_handle(&id))
                    .unwrap()
                    .as_record_id(),
                &id
            );
        }
        assert_ne!(
            format_project_handle(&RecordId::new("project", 7_i64)),
            format_project_handle(&RecordId::new("project", "7"))
        );
        assert_ne!(
            format_project_handle(&RecordId::new("project", uuid)),
            format_project_handle(&RecordId::new("project", UUID_TEXT))
        );
        // Equivalent alternate point-literal form: the parenthesis branch of `parse_value` is a
        // valid way to spell the same point, and full-input parsing must not narrow it away.
        assert_eq!(
            parse_project_handle("(project:7)").unwrap().as_record_id(),
            &RecordId::new("project", 7_i64)
        );
    }

    #[test]
    fn project_selector_keeps_aliases_explicit_without_reclassifying_invalid_handles() {
        assert!(parse_project_selector("ordinary-alias").unwrap().is_none());
        assert!(parse_project_selector("project:7").unwrap().is_some());
        for text in ["project:", "project:1..9", "workspace:7", "fact:7"] {
            assert!(
                parse_project_selector(text).is_err(),
                "not an alias fallback: {text:?}"
            );
        }
    }

    #[test]
    fn project_handle_refuses_nonpoint_and_nonproject_values() {
        for text in [
            "",
            "alias",
            "workspace:7",
            "fact:7",
            "project:1..9",
            "[project:7]",
            "{ project: project:7 }",
            "type::record('project', 7)",
            "project:7; project:8",
            "(project:7) project:8",
            "(project:7) ;",
        ] {
            let parsed = parse_project_handle(text);
            assert!(
                parsed.is_err(),
                "not a project point literal: {text:?}; parsed result: {parsed:?}"
            );
        }
    }

    #[test]
    fn record_id_parse_refuses_garbage_and_preserves_key_types() {
        // Trailing input and non-point values are refused outright — under `parse_simple` each
        // of these silently named a different record (an invented string key, a range scan, an
        // expression).
        for text in [
            "",
            "junk",
            "fact:x; junk",
            "fact:x ;",
            "fact:7 fact:8",
            "fact:1..9",
            "[fact:7]",
            "type::record('fact', 7)",
        ] {
            let parsed = parse_record_id(text);
            assert!(
                parsed.is_err(),
                "not a single record point literal: {text:?}; parsed result: {parsed:?}"
            );
        }
        // Key types parse as themselves: the numeric/string distinction `parse_simple` flattened,
        // and the quoted and uuid forms the display round trip depends on.
        let uuid = "123e4567-e89b-42d3-a456-426614174000"
            .parse::<surrealdb::types::Uuid>()
            .unwrap();
        for id in [
            RecordId::new("fact", 7_i64),
            RecordId::new("fact", "7"),
            RecordId::new("fact", "space key"),
            RecordId::new("fact", uuid),
        ] {
            assert_eq!(parse_record_id(&id.to_sql()).unwrap(), id);
        }
        // The equivalent alternate literal form stays accepted, as it does for project handles.
        assert_eq!(
            parse_record_id("(fact:7)").unwrap(),
            RecordId::new("fact", 7_i64)
        );
        // A bare alphanumeric key keeps parsing as the string key it always was.
        assert_eq!(
            parse_record_id("fact:abc").unwrap(),
            RecordId::new("fact", "abc")
        );
    }
}
