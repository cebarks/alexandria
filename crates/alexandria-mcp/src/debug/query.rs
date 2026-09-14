//! The Query Tester: a form page plus the htmx fragment that answers it.
//!
//! Both live in templates: `query.html` is a normal page, `query_results.html` is a body
//! fragment (it does not extend `layout.html`). Because `run` only ever answers with a fragment,
//! its error paths are `Fragment::Error` — `html::error_page` would wrap the message in a whole
//! layout and swap a second `<nav>` into `#query-results`, which is not what this endpoint has
//! ever returned.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::Response;
use serde::Deserialize;

use super::html::page;
use crate::AlexandriaServer;
use crate::tools::{RecallParams, RetrieveMemoriesParams};

/// The form. `nav` is the only field: no values are pre-filled, the inputs are static markup
/// whose `name` attributes are what [`QueryForm`] deserializes.
#[derive(Template)]
#[template(path = "query.html")]
struct QueryTemplate {
    nav: &'static str,
}

pub async fn form(State(_server): State<AlexandriaServer>) -> Response {
    page(QueryTemplate { nav: "query" })
}

#[derive(Debug, Deserialize)]
pub struct QueryForm {
    pub mode: String,
    pub query: String,
    /// Retrieve-only: `RecallParams` has no limit field.
    ///
    /// A raw string, normalized in [`run`] like the other optional fields, rather than an
    /// `Option<usize>`: the shipped `<input type="number" name="limit">` posts `limit=` when the
    /// operator clears the box, and `serde_urlencoded` cannot parse an empty string into a number.
    /// A typed field would make axum's `Form` extractor reject the whole submission with a 422 —
    /// and htmx 2 does not swap a 4xx body into `#query-results`, so the page would look like Run
    /// did nothing at all. Blank means "no opinion", not "malformed request".
    pub limit: Option<String>,
    /// Retrieve-only, and the *external* session id — the same value `store_memory(session_id)`
    /// takes, because that is what `SessionRepo::get_memories` keys on. An `<input type="text">`
    /// left blank posts the key with an empty value, so this is `Some("")` rather than `None`
    /// when the operator does not fill it in; [`run`] normalizes that away.
    pub session_id: Option<String>,
    /// Retrieve-only. The template posts this as `<input type="checkbox" value="true">`, which is
    /// **absent from the body entirely** when unchecked rather than present-and-empty, so serde
    /// gives `Some("true")` or `None` — never `Some(false)`. [`run`] normalizes that pair down to
    /// a `bool` at the point of use, the same shape `memories::list` uses for its `include_deleted`
    /// checkbox. Deliberately not a field on `RetrieveMemoriesParams`, which is the public MCP tool
    /// schema.
    pub dry_run: Option<String>,
}

/// The model the bands below were measured on, as the Query Tester's legend names it.
///
/// Held apart from [`SCORE_BANDS_LEGEND`] only because the template wraps it in `<code>`, and
/// markup cannot come from a const (`|safe` is forbidden in this project — askama escapes every
/// `{{ }}`, so the only remaining decision is context, not trustworthiness).
pub const SCORE_BANDS_MODEL: &str = "all-MiniLM-L6-v2";

/// The Query Tester's score-band legend: one sentence, one source.
///
/// This was static prose duplicated between `templates/query_results.html` and
/// `docs/configuration.md`, which meant a re-measurement that updated one copy passed CI and left
/// the operator's documentation contradicting the UI. Now the template renders this const, and
/// `test_configuration_md_quotes_every_score_band` fails if `docs/configuration.md` stops quoting
/// the same numbers.
pub const SCORE_BANDS_LEGEND: &str = "measured 2026-09-08 -- model-dependent, so re-measure for any \
     other embedding model: keyword or near-paraphrase hit 0.55-0.76, natural-language question \
     against its matching statement 0.40-0.65, a question sharing no vocabulary with the statement \
     as low as ~0.2, unrelated memories 0.07-0.40.";

/// The four ranges inside [`SCORE_BANDS_LEGEND`], spelled as the legend spells them.
///
/// Not a second copy of the measurement: `test_score_band_ranges_are_in_the_legend` asserts each
/// string occurs in the legend, so this array is an *index* into it rather than a parallel source.
/// It exists so the docs cross-check can name every band without parsing prose, and so the
/// rendered-fragment test covers all four rather than the three it happened to list.
pub const SCORE_BAND_RANGES: &[&str] = &["0.55-0.76", "0.40-0.65", "~0.2", "0.07-0.40"];

/// One row of the ID / Content / Similarity table. `retrieve_memories` results and focused
/// recall share this shape — the legacy code rendered both with the same `format!` line.
#[derive(Clone)]
struct ResultRow {
    id: String,
    content: String,
    /// `score` as the legacy table printed it (`{:.4}`). Askama renders an `f64` with plain
    /// `Display`, which would print `0.5` where the table printed `0.5000`.
    similarity: String,
    /// The same number as a value, for the floor comparison. `similarity` is display-only: a
    /// truncated string cannot be compared against `retrieve_min_similarity` without inventing
    /// rounding rules, so the comparison happens on what the tool itself compared.
    score: f32,
}

/// What one retrieve run decided: the rows, and the settings that produced them.
struct RetrieveReport {
    /// `retrieve.min_similarity` in force, printed to two places like the config does.
    floor: String,
    /// `activation.top_n` in force.
    top_n: usize,
    kept: Vec<ResultRow>,
    dropped: Vec<ResultRow>,
    /// Counts the window honestly — "of the top N ranked, M fell below the floor", never "all
    /// dropped results", because the tool truncates to `limit` before filtering.
    window_note: String,
    /// Whether the heat write happened, and which ids seeded it.
    activation_note: String,
    /// Set exactly when `kept` is empty, so the table is replaced by a line that says why.
    empty_note: Option<&'static str>,
}

/// One cluster of a broad recall. Only the representative memory *contents* are listed: the
/// legacy renderer never showed the per-memory ids or similarities inside the `<ul>`, so this
/// drops nothing.
struct RecallCluster {
    id: String,
    similarity: String,
    contents: Vec<String>,
}

/// What `/debug/query/run` can answer — one variant per return branch of the two `format!`
/// renderers this replaces. An enum rather than a struct of optional fields so the fragment
/// cannot render two branches at once, and so the template's `{% match %}` is checked for
/// exhaustiveness at compile time.
enum Fragment {
    /// A tool error, an unknown `mode` value, or a recall payload that failed to parse.
    Error { message: String },
    /// A bare-`<p>` empty state ("No results." and friends), which legacy had no class for.
    Notice { message: String },
    /// The results table. `heading` is `Some("Focused recall")` for recall's focused mode;
    /// retrieve results had no heading at all.
    Table {
        heading: Option<&'static str>,
        rows: Vec<ResultRow>,
    },
    /// Broad recall: one section per cluster. The `<h3>Broad recall</h3>` lives in the template.
    Clusters { sections: Vec<RecallCluster> },
    /// A retrieve run, with the floor / activation context that makes "why didn't this find X?"
    /// answerable from the fragment alone. A struct variant because askama rejects `{ Variant { f } }`
    /// syntax for tuple variants.
    Retrieve { report: RetrieveReport },
}

impl Fragment {
    fn error(message: String) -> Self {
        Self::Error { message }
    }

    fn notice(message: &str) -> Self {
        Self::Notice {
            message: message.to_string(),
        }
    }

    /// A `recall` result. The tool hands back a JSON *string*, so it can still fail to parse
    /// here after a successful call — that is this function's own `Error` branch.
    fn recall(json_str: &str) -> Self {
        let value: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => return Self::error(format!("Failed to parse recall response: {e}")),
        };

        // The form never sends a scope handle, so in practice only "broad" arrives here, but
        // both branches are kept: a missing or unexpected `mode` falls through to broad, which
        // is what legacy did (`unwrap_or("")`), and broad with no clusters prints
        // "No clusters found." rather than an empty table.
        if value.get("mode").and_then(|v| v.as_str()).unwrap_or("") == "focused" {
            let rows = result_rows(value.get("memories"));
            if rows.is_empty() {
                Self::notice("No memories in this scope.")
            } else {
                Self::Table {
                    heading: Some("Focused recall"),
                    rows,
                }
            }
        } else {
            let empty = Vec::new();
            let clusters = value
                .get("clusters")
                .and_then(|c| c.as_array())
                .unwrap_or(&empty);
            let sections: Vec<RecallCluster> = clusters
                .iter()
                .map(|c| RecallCluster {
                    id: string_field(c, "cluster_id"),
                    similarity: similarity_field(c),
                    contents: c
                        .get("representative_memories")
                        .and_then(|m| m.as_array())
                        .unwrap_or(&empty)
                        .iter()
                        .map(|m| string_field(m, "content"))
                        .collect(),
                })
                .collect();
            if sections.is_empty() {
                Self::notice("No clusters found.")
            } else {
                Self::Clusters { sections }
            }
        }
    }
}

/// Rows for a JSON array of `{ id, content, similarity }` entries. A missing or non-array value
/// yields no rows, which the callers turn into their empty state — as legacy did.
fn result_rows(value: Option<&serde_json::Value>) -> Vec<ResultRow> {
    let empty = Vec::new();
    let entries = value.and_then(|v| v.as_array()).unwrap_or(&empty);
    entries
        .iter()
        .map(|entry| {
            let score = similarity_value(entry);
            ResultRow {
                id: string_field(entry, "id"),
                content: string_field(entry, "content"),
                similarity: format!("{score:.4}"),
                score,
            }
        })
        .collect()
}

/// A string field, empty when absent or not a string — the legacy renderers used
/// `unwrap_or("")` rather than skipping a malformed entry, so a bad row shows blank cells.
fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A `similarity` as a number, or 0.0 when absent — the same default the legacy renderers printed.
/// Read from the JSON `f64` the tool serialized from an `f32`, so it round-trips back to the
/// exact value `retrieve_core` compared against the floor.
fn similarity_value(value: &serde_json::Value) -> f32 {
    value
        .get("similarity")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as f32
}

/// A `similarity` as the legacy `{:.4}` printed it. Formatting stays in Rust: askama renders an
/// `f64` with plain `Display`, which would print `0.5` where the table printed `0.5000`.
fn similarity_field(value: &serde_json::Value) -> String {
    format!("{:.4}", similarity_value(value))
}

/// The fragment template. No `nav` field, because `query_results.html` is swapped into a div
/// rather than extending `layout.html`.
#[derive(Template)]
#[template(path = "query_results.html")]
struct QueryResultsTemplate {
    fragment: Fragment,
    /// Standing note about what the selected mode ignores, rendered above the fragment. See
    /// [`RECALL_IGNORES_NOTE`] for why it is a constant rather than a per-field warning.
    mode_note: Option<&'static str>,
}

/// Named on every recall fragment, always the same sentence. It *could* be conditional on the
/// operator having filled a retrieve-only field in, but a conditional notice would then be naming
/// `dry_run` while never being triggered by `dry_run` — and the byte-equality of two recall
/// fragments that differ only in `dry_run` is what `test_query_run_recall_ignores_dry_run` pins.
/// A constant line cannot be wrong and cannot break that property.
const RECALL_IGNORES_NOTE: &str = "recall ignores limit, session_id and dry_run — it takes only a query and an optional scope handle.";

/// One honest sentence about how many rows the ranked window held and how many the floor took.
/// `window` is the tool's top-`limit` slice, not the database, so the wording says "ranked".
fn window_note(window: usize, dropped: usize, floor: &str) -> String {
    if window == 0 {
        return "Nothing was ranked: the query loaded no memories to score.".to_string();
    }
    if dropped == 0 {
        return format!(
            "Ranked window: the top {window}. None of them fell below the server-side floor {floor}."
        );
    }
    format!(
        "Ranked window: the top {window}. {dropped} of them fell below the server-side floor {floor}."
    )
}

/// One honest sentence about the spreading-activation write. It is the only side effect a retrieve
/// run can have, so the fragment has to say whether it happened — and "happened" here means
/// `retrieve_core` seeded activation from these ids, which only moves heat where an edge exists.
fn activation_note(wet: bool, kept: &[ResultRow], top_n: usize) -> String {
    if !wet {
        return "Spreading activation did not run — this retrieval wrote nothing.".to_string();
    }
    let seeds: Vec<String> = kept.iter().take(top_n).map(|row| row.id.clone()).collect();
    if seeds.is_empty() {
        return "Spreading activation did not run: there was no kept result to seed it from."
            .to_string();
    }
    let listed = seeds.join(", ");
    format!(
        "Spreading activation ran, seeded from {} of {} kept results: {listed}. Where a seed has \
         graph neighbours, their heat increased.",
        seeds.len(),
        kept.len()
    )
}

/// The retrieve branch of [`run`].
///
/// Two reads, on purpose:
///
/// 1. The kept table is whatever the path the operator selected returns — `do_retrieve_memories`
///    when not dry (the real tool, heat write included), `do_retrieve_memories_dry` when dry. That
///    is what "what would this query return" means here: the tool's own answer, not a reconstruction
///    the debug layer could get wrong.
/// 2. The unfiltered window (`do_retrieve_memories_unfiltered`, same ranking code, floor off, no
///    writes) supplies the rows the floor suppressed, on every run — not just dry ones — so the
///    kept list and the dropped list describe one ranking.
///
/// Reimplementing activation to avoid read 1 on a wet run would put an unsanctioned second write
/// path in the debug layer, and recomputing `kept` from the window instead of asking the tool would
/// let the two disagree silently. Both are avoided.
async fn retrieve_fragment(
    server: &AlexandriaServer,
    query: String,
    limit: Option<usize>,
    session_id: Option<String>,
    dry_run: bool,
) -> Fragment {
    // Built per call rather than moved, because `RetrieveMemoriesParams` is the tool's own params
    // type and does not (and should not) derive `Clone`.
    let params = || RetrieveMemoriesParams {
        query: query.clone(),
        limit,
        session_id: session_id.clone(),
    };
    let kept = match if dry_run {
        server.do_retrieve_memories_dry(params()).await
    } else {
        server.do_retrieve_memories(params()).await
    } {
        Ok(value) => result_rows(value.get("results")),
        Err(e) => return Fragment::error(e.to_string()),
    };
    let window = match server.do_retrieve_memories_unfiltered(params()).await {
        Ok(value) => result_rows(value.get("results")),
        Err(e) => return Fragment::error(e.to_string()),
    };

    let floor = server.retrieve_min_similarity;
    // Below the floor *and* absent from the kept table, so a row can never be shown twice if the
    // two paths ever disagree about the comparison.
    let dropped: Vec<ResultRow> = window
        .iter()
        .filter(|row| row.score < floor)
        .filter(|row| !kept.iter().any(|kept| kept.id == row.id))
        .cloned()
        .collect();

    let floor_text = format!("{floor:.2}");
    let empty_note = match (kept.is_empty(), window.is_empty()) {
        (false, _) => None,
        // Nothing was loaded at all, which is a different fact from "everything was filtered".
        (true, true) => Some("No results."),
        (true, false) => Some("No results above the floor — see the dropped section below."),
    };
    let report = RetrieveReport {
        window_note: window_note(window.len(), dropped.len(), &floor_text),
        activation_note: activation_note(!dry_run, &kept, server.activation_top_n),
        floor: floor_text,
        top_n: server.activation_top_n,
        empty_note,
        kept,
        dropped,
    };
    Fragment::Retrieve { report }
}

pub async fn run(State(server): State<AlexandriaServer>, Form(form): Form<QueryForm>) -> Response {
    let mode_note = matches!(form.mode.as_str(), "recall").then_some(RECALL_IGNORES_NOTE);
    let fragment = match form.mode.as_str() {
        "retrieve" => {
            // A checkbox sends `dry_run=true` when checked and nothing at all when not, so
            // "present and `true`" is the whole yes test — and anything else a hand-crafted body
            // could carry (`dry_run=1`) reads as not-dry, which is the safe direction to guess:
            // it leaves the run doing what the real tool does rather than quietly suppressing a
            // write the operator did not ask to suppress.
            let dry_run = form.dry_run.as_deref() == Some("true");
            // A blank text input still submits `session_id=`, which deserializes to `Some("")`
            // (and `Some("   ")` for spaces). Scoping to that would walk the edges of a session
            // whose external id is empty, find nothing, and report "No results." — a false
            // negative that looks like retrieval being broken rather than like an unfilled form
            // field, which is exactly the confusion this page exists to remove. So: blank means
            // unscoped.
            let session_id = form.session_id.filter(|id| !id.trim().is_empty());
            // Same shape as the two above: an empty box and an unparseable box both mean "the
            // operator expressed no preference", so they normalize to `None` and `do_retrieve_memories`
            // applies its own default (`params.limit.unwrap_or(10)`). Erroring here would be a 4xx
            // that htmx cannot render — a silent dead end on the page whose whole job is to explain
            // why a query returned what it returned.
            let limit = form
                .limit
                .as_deref()
                .and_then(|raw| raw.trim().parse::<usize>().ok());
            // Spreading activation writes heat, so a retrieve run is the one debug route that
            // mutates. The template says so out loud; the checkbox routes around it for an
            // operator who wants an answer that a previous test query did not bias.
            retrieve_fragment(&server, form.query, limit, session_id, dry_run).await
        }
        // recall mode intentionally ignores all three of the form's retrieve-only fields:
        //   - `limit`: `RecallParams` has no limit field at all;
        //   - `session_id`: recall's `scope_handle` is an opaque handle returned by a previous
        //     broad recall that narrows into one cluster — a different concept from a session id,
        //     so there is nothing honest to map a session onto;
        //   - `dry_run`: worth stating before anyone adds a branch, because `do_recall` writes
        //     nothing at all (no activation, no heat), so there is no side effect left for a dry
        //     run to skip.
        // The form labels all three "(retrieve only)" and, since there is no JavaScript here to
        // hide them, `mode_note` states it again in the fragment itself. That note is a constant:
        // see [`RECALL_IGNORES_NOTE`] for why it cannot be conditional.
        "recall" => {
            let params = RecallParams {
                query: form.query.clone(),
                scope_handle: None,
            };
            match server.do_recall(params).await {
                Ok(json_str) => Fragment::recall(&json_str),
                Err(e) => Fragment::error(e.to_string()),
            }
        }
        other => Fragment::error(format!("Unknown mode: {other}")),
    };
    page(QueryResultsTemplate {
        fragment,
        mode_note,
    })
}

#[cfg(test)]
mod tests {
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_query_run_retrieve_mode_empty_db() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=retrieve&query=test&limit=5"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("No results"));
    }

    #[tokio::test]
    async fn test_query_run_retrieve_mode_finds_stored_fact() {
        let server = super::super::test_support::test_server().await;
        let repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        repo.create_fact("findable memory", 0.5, &[0.1, 0.2], &[])
            .await
            .unwrap();

        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=retrieve&query=test&limit=5"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("findable memory"));
    }

    #[tokio::test]
    async fn test_query_run_recall_mode_without_limit() {
        let server = super::super::test_support::test_server().await;
        let app = crate::debug::router(server);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("mode=recall&query=test"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("No clusters found") || text.contains("Broad recall"));
    }

    /// POST a urlencoded form body to `/debug/query/run` and return the fragment it swapped in.
    /// `Router` is consumed by `oneshot`, so callers pass a clone — same shape as the `fetch_body`
    /// helper in `sessions.rs`.
    async fn run_form(app: axum::Router, body: &'static str) -> String {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/query/run")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// Two sessions, each carrying exactly one memory of its own, so a scoped retrieval has
    /// something it must return and something it must not.
    async fn seed_two_sessions(server: &crate::AlexandriaServer) {
        let session_repo = alexandria_storage::repos::SessionRepo::new(server.db.inner());
        let memory_repo = alexandria_storage::repos::MemoryRepo::new(server.db.inner());
        for (external_id, content) in [
            ("sess-alpha", "alpha project decision"),
            ("sess-beta", "beta project decision"),
        ] {
            // `create` hands back the internal record id (what `add_memory` wants); the form
            // posts the *external* id, which is what `store_memory(session_id)` uses.
            let sess = session_repo.create(external_id, None, None).await.unwrap();
            let fact = memory_repo
                .create_fact(content, 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
            session_repo.add_memory(&sess, &fact).await.unwrap();
        }
    }

    /// `</tr>` occurrences = 1 header row + N result rows, which pins the row count rather than
    /// just testing for a substring's presence/absence. Saturating so a fragment with no table at
    /// all (the `<p>No results.</p>` empty state) reports 0 and lets the assertion print the
    /// fragment, rather than panicking on a subtract overflow first.
    fn row_count(fragment: &str) -> usize {
        fragment.matches("</tr>").count().saturating_sub(1)
    }

    /// One result fragment per line, sorted, so two submissions are compared on *which* rows came
    /// back rather than the order they came back in. `do_retrieve_memories` ranks with a stable
    /// sort, so a tie — every embedding in these tests is the identical stub vector — preserves
    /// whatever order SurrealDB's `SELECT * FROM fact` scan returned. That is stable inside one
    /// process but is not a guarantee across a restart, and row sequence carries nothing worth
    /// asserting here anyway.
    fn sorted_lines(fragment: &str) -> String {
        let mut lines: Vec<&str> = fragment.lines().collect();
        lines.sort_unstable();
        lines.join("\n")
    }

    #[tokio::test]
    async fn test_query_tester_scopes_retrieve_to_session() {
        let server = super::super::test_support::test_server().await;
        seed_two_sessions(&server).await;
        let app = crate::debug::router(server);

        let scoped = run_form(
            app.clone(),
            "mode=retrieve&query=project+decision&limit=10&session_id=sess-alpha",
        )
        .await;
        assert!(
            scoped.contains("alpha project decision"),
            "the scoped session's own memory must be returned; got: {scoped}"
        );
        assert!(
            !scoped.contains("beta project decision"),
            "another session's memory must not leak into a session-scoped retrieval; got: {scoped}"
        );
        assert_eq!(
            row_count(&scoped),
            1,
            "scoped retrieval must return exactly one row; got: {scoped}"
        );

        // Unscoped, both must appear. Without this half the assertions above would also pass if
        // the query or the similarity floor were what dropped session B.
        let unscoped = run_form(app, "mode=retrieve&query=project+decision&limit=10").await;
        assert!(
            unscoped.contains("alpha project decision"),
            "the scoped session's memory must still be visible unscoped; got: {unscoped}"
        );
        assert!(
            unscoped.contains("beta project decision"),
            "an unscoped retrieval must see every session's memory; got: {unscoped}"
        );
        assert_eq!(
            row_count(&unscoped),
            2,
            "the same query without a session must return both rows; got: {unscoped}"
        );
    }

    #[tokio::test]
    async fn test_query_tester_empty_session_id_is_unscoped() {
        let server = super::super::test_support::test_server().await;
        seed_two_sessions(&server).await;
        let app = crate::debug::router(server);

        let omitted = run_form(app.clone(), "mode=retrieve&query=project+decision&limit=10").await;
        // A blank `<input type="text">` still posts the key, so the handler receives
        // `Some("")` (and `Some("  ")` for whitespace) rather than `None`.
        let empty = run_form(
            app.clone(),
            "mode=retrieve&query=project+decision&limit=10&session_id=",
        )
        .await;
        let whitespace = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&session_id=%20%20",
        )
        .await;

        for fragment in [&empty, &whitespace] {
            assert!(
                fragment.contains("alpha project decision")
                    && fragment.contains("beta project decision"),
                "a blank session id must run unscoped, not against a session whose external id is \
                 empty; got: {fragment}"
            );
        }
        assert_eq!(
            sorted_lines(&empty),
            sorted_lines(&omitted),
            "an empty session_id= must produce exactly the unscoped results"
        );
        assert_eq!(
            sorted_lines(&whitespace),
            sorted_lines(&omitted),
            "a whitespace-only session_id must produce exactly the unscoped results"
        );
    }

    /// A cleared `<input type="number" name="limit">` posts `limit=`, which the typed `Option<usize>`
    /// this field used to be could not deserialize — axum's `Form` extractor answered 422 and htmx
    /// swapped nothing, so Run looked dead. Blank and junk must both read as "no opinion" and get
    /// the tool's own default (`limit.unwrap_or(10)`), which is what the omitted-limit and the
    /// explicit `limit=3` controls below pin down.
    #[tokio::test]
    async fn test_query_tester_blank_or_junk_limit_falls_back_to_default() {
        let server = super::super::test_support::test_server().await;
        // More facts than the default limit, so "default" is a countable outcome rather than an
        // unfalsifiable one: 12 stored, 10 shown. StubEmbedding scores every pair 1.0, so the
        // floor drops none of them and the row count is purely the limit.
        for i in 0..12 {
            server
                .do_store_memory(crate::tools::StoreMemoryParams {
                    content: format!("pinned memory number {i}"),
                    tags: None,
                    session_id: None,
                })
                .await
                .unwrap();
        }
        let app = crate::debug::router(server);

        let blank = run_form(app.clone(), "mode=retrieve&query=pinned+memory&limit=").await;
        let junk = run_form(app.clone(), "mode=retrieve&query=pinned+memory&limit=abc").await;
        let omitted = run_form(app.clone(), "mode=retrieve&query=pinned+memory").await;
        let explicit = run_form(app.clone(), "mode=retrieve&query=pinned+memory&limit=3").await;

        for fragment in [&blank, &junk, &omitted] {
            assert_eq!(
                row_count(fragment),
                10,
                "a blank or unparseable limit must yield the server default of 10 rows; got: {fragment}"
            );
            assert!(
                fragment.contains("Ranked window: the top 10."),
                "the window note must report the default window; got: {fragment}"
            );
        }
        // Positive control: the fixture can produce a different count, so the 10 above is the
        // default limit at work and not a cap on what these tests can ever show.
        assert_eq!(
            row_count(&explicit),
            3,
            "an explicit limit must still be honoured; got: {explicit}"
        );
    }

    /// The two handler tests above post the wire format, so they cannot see the template half of
    /// the contract: if an input's `name` drifted from a `QueryForm` field the field would simply
    /// stop being submittable and every handler test would still pass. This renders the form.
    #[test]
    fn test_query_form_template_names_every_query_form_field() {
        let html = super::QueryTemplate { nav: "query" }.render().unwrap();

        assert!(
            html.contains(r#"class="active">Query Tester"#),
            "layout must highlight the Query Tester nav entry; got: {html}"
        );
        assert!(
            html.contains(r##"hx-post="/debug/query/run" hx-target="#query-results""##),
            "the swap targets are part of the contract with `run`; got: {html}"
        );
        // One `name` per QueryForm field, so the struct stays submittable in full.
        for name in ["mode", "query", "limit", "session_id", "dry_run"] {
            assert!(
                html.contains(&format!(r#"name="{name}""#)),
                "`name=\"{name}\"` missing from the form; got: {html}"
            );
        }
        // Both retrieve-only fields must say so on the label, since neither is hidden when
        // mode=recall is selected.
        assert!(
            html.contains("Limit (retrieve only)") && html.contains("Session ID (retrieve only)"),
            "fields recall ignores must be labelled retrieve-only; got: {html}"
        );
        assert!(
            html.contains(r##"name="session_id" placeholder="external session id,"##),
            "the session id input must be free text hinting at the expected value; got: {html}"
        );
        // Nothing is pre-filled — `run` answers with a fragment only, so this form is never
        // re-rendered after a submission. A `value="{{ ... }}"` here would be a lie.
        assert!(
            !html.contains(r#"session_id" value="#),
            "the form must not pretend to persist submissions; got: {html}"
        );
        // The dry-run control must be a checkbox carrying exactly the value [`QueryForm`] expects
        // to see when it is present: a text input posting "" would make the absent-vs-empty
        // distinction the handler relies on unreachable through the real form.
        assert!(
            html.contains(r##"type="checkbox" name="dry_run" value="true""##),
            "dry run must be a checkbox posting `true`; got: {html}"
        );
        // Recall writes nothing, so the knob means nothing there — the label has to say so.
        assert!(
            html.contains("Dry run (retrieve only)"),
            "the dry-run label must be marked retrieve-only; got: {html}"
        );
        // The mutating default is surfaced without the operator having to read `server.rs`.
        assert!(
            html.contains("it bumps heat on the top-ranked results"),
            "the form must warn that a non-dry retrieve run writes heat; got: {html}"
        );
        assert!(
            !html.contains(r#"name="dry_run" value="true" checked"#),
            "dry run must start unchecked, so the tester keeps the real tool's behaviour; got: {html}"
        );
    }

    // --- The dry-run checkbox, end to end ---------------------------------------
    //
    // `server.rs` already proves `do_retrieve_memories_dry` does not write heat. The paired
    // wet/dry tests below prove the *form field* is what selects it: a handler that read `dry_run`
    // into a variable and then called the activating path anyway would pass every server-level
    // test while lying to the operator.

    /// A two-memory graph with a real edge between them, which is what spreading activation walks.
    /// Same construction the server-level fixture uses: `create_fact` alone is not enough, because
    /// `HeatRepo::add_heat` updates an existing `heat_state` row and silently matches nothing when
    /// there is none — so these go through `do_store_memory`, which creates one.
    async fn seed_edge_pair(server: &crate::AlexandriaServer) -> (String, String) {
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
        let mut ids = Vec::new();
        for content in ["alpha project decision", "beta project decision"] {
            // `do_store_memory` so each fact gets its `heat_state` row; the stub embedding makes
            // every similarity 1.0, so both memories are always above the retrieve floor.
            ids.push(
                server
                    .do_store_memory(crate::tools::StoreMemoryParams {
                        content: content.to_string(),
                        tags: None,
                        session_id: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        edge_repo
            .create_edge(&ids[0], &ids[1], "relates_to", 1.0)
            .await
            .unwrap();
        (ids[0].clone(), ids[1].clone())
    }

    /// Heat of both seeded memories. Read through `HeatRepo` — no raw query from a route test.
    async fn seeded_heat(
        server: &crate::AlexandriaServer,
        ids: &(String, String),
    ) -> (Option<f64>, Option<f64>) {
        let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
        (
            heat_repo.get(&ids.0).await.unwrap().map(|h| h.heat),
            heat_repo.get(&ids.1).await.unwrap().map(|h| h.heat),
        )
    }

    /// The control: the default (non-dry) submission must still behave like the real tool, heat
    /// write included, and it is what stops the dry assertion below passing on a fixture that
    /// cannot activate at all.
    #[tokio::test]
    async fn test_query_run_retrieve_writes_heat_without_dry_run() {
        let server = super::super::test_support::test_server().await;
        let ids = seed_edge_pair(&server).await;
        let before = seeded_heat(&server, &ids).await;
        assert_eq!(
            before,
            (Some(1.0), Some(1.0)),
            "each seeded memory starts at heat 1.0"
        );

        let app = crate::debug::router(server.clone());
        let fragment = run_form(app, "mode=retrieve&query=project+decision&limit=10").await;
        assert!(
            fragment.contains("alpha project decision")
                && fragment.contains("beta project decision"),
            "the query must actually retrieve the seeded pair; got: {fragment}"
        );

        let after = seeded_heat(&server, &ids).await;
        // One hop at the default propagation factor: 1.0 * 0.3^1 * edge strength 1.0.
        let warmed = |h: Option<f64>| h.is_some_and(|h| (h - 1.3).abs() < 1e-3);
        assert!(
            warmed(after.0) && warmed(after.1),
            "a non-dry retrieve run must warm both ends of the edge, exactly as \
             retrieve_memories does; before {before:?}, after {after:?}"
        );
    }

    /// `dry_run=true` in the form body must reach `do_retrieve_memories_dry`.
    #[tokio::test]
    async fn test_query_run_retrieve_dry_run_does_not_write_heat() {
        let server = super::super::test_support::test_server().await;
        let ids = seed_edge_pair(&server).await;
        let before = seeded_heat(&server, &ids).await;

        let app = crate::debug::router(server.clone());
        let fragment = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&dry_run=true",
        )
        .await;
        assert!(
            fragment.contains("alpha project decision")
                && fragment.contains("beta project decision"),
            "dry must still run the retrieval and show its results; got: {fragment}"
        );
        assert_eq!(
            seeded_heat(&server, &ids).await,
            before,
            "a dry retrieve run must leave heat exactly where it was"
        );
    }

    /// Recall writes nothing, so the checkbox must not change its behaviour: same results with
    /// and without it. If a dry branch were ever added to recall, this is the test that says it
    /// bought nothing.
    #[tokio::test]
    async fn test_query_run_recall_ignores_dry_run() {
        let server = super::super::test_support::test_server().await;
        server
            .do_store_memory(crate::tools::StoreMemoryParams {
                content: "a recallable fact".to_string(),
                tags: None,
                session_id: None,
            })
            .await
            .unwrap();
        let app = crate::debug::router(server.clone());

        let plain = run_form(app.clone(), "mode=recall&query=recallable+fact").await;
        let dry = run_form(app, "mode=recall&query=recallable+fact&dry_run=true").await;
        assert_eq!(
            sorted_lines(&plain),
            sorted_lines(&dry),
            "recall writes nothing, so dry_run must be a no-op there"
        );
    }

    // --- Why didn't this find X? floor, dropped rows, activation ----------------
    //
    // These run on `test_support::banded_server()`, not `test_server()`: `StubEmbedding` returns
    // one constant vector, so every similarity there is 1.0 and the floor can never drop
    // anything. With the banded stub, "strong" scores 0.60 and "weak" 0.20 against a 0.30 floor.

    /// Two memories that straddle the floor, stored through the server so each gets a
    /// `heat_state` row. Returns their ids in (strong, weak) order.
    async fn seed_straddling_pair(server: &crate::AlexandriaServer) -> (String, String) {
        let mut ids = Vec::new();
        for content in ["a strong match memory", "a weak match memory"] {
            ids.push(
                server
                    .do_store_memory(crate::tools::StoreMemoryParams {
                        content: content.to_string(),
                        tags: None,
                        session_id: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        (ids[0].clone(), ids[1].clone())
    }

    /// Split a fragment at the dropped section, so an assertion can say *which* table a row
    /// appeared in. `None` when there is no dropped section at all.
    fn split_at_dropped(fragment: &str) -> Option<(&str, &str)> {
        fragment.split_once("Dropped by min_similarity")
    }

    #[tokio::test]
    async fn test_query_tester_shows_floor_and_dropped_rows() {
        let server = super::super::test_support::banded_server().await;
        seed_straddling_pair(&server).await;
        let app = crate::debug::router(server);

        let fragment = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&dry_run=true",
        )
        .await;

        // The settings that decided the answer, labelled as the server's own.
        assert!(
            fragment.contains("<code>retrieve.min_similarity</code> = <strong>0.30</strong>"),
            "the effective floor must be rendered above the results; got: {fragment}"
        );
        assert!(
            fragment.contains("<code>activation.top_n</code> = <strong>3</strong>"),
            "activation.top_n must be shown next to it; got: {fragment}"
        );

        let Some((kept, dropped)) = split_at_dropped(&fragment) else {
            panic!("a below-floor row must produce a dropped section; got: {fragment}");
        };
        assert!(
            kept.contains("a strong match memory") && kept.contains("0.6000"),
            "the above-floor memory stays in the main table; got: {kept}"
        );
        assert!(
            !kept.contains("a weak match memory"),
            "the suppressed memory must not be presented as a kept result; got: {kept}"
        );
        assert!(
            dropped.contains("a weak match memory") && dropped.contains("0.2000"),
            "the suppressed memory must appear in the dropped section with its score; got: {dropped}"
        );
        // Honest about the window: the floor only ever sees the top `limit` rows.
        assert!(
            fragment.contains(
                "Ranked window: the top 2. 1 of them fell below the server-side floor 0.30."
            ),
            "the fragment must count the window rather than claim to show every dropped row; got: {fragment}"
        );
        assert!(
            fragment.contains("this is not every suppressed memory in the database"),
            "the top-`limit`-before-floor caveat must be stated; got: {fragment}"
        );
    }

    #[tokio::test]
    async fn test_query_tester_has_no_dropped_section_when_floor_drops_nothing() {
        let server = super::super::test_support::banded_server().await;
        server
            .do_store_memory(crate::tools::StoreMemoryParams {
                content: "a strong match memory".to_string(),
                tags: None,
                session_id: None,
            })
            .await
            .unwrap();
        let app = crate::debug::router(server);

        let fragment = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&dry_run=true",
        )
        .await;
        assert!(
            split_at_dropped(&fragment).is_none(),
            "nothing was below the floor, so no dropped section may appear; got: {fragment}"
        );
        assert_eq!(
            row_count(&fragment),
            1,
            "exactly the one kept row; got: {fragment}"
        );
        assert!(
            fragment.contains(
                "Ranked window: the top 1. None of them fell below the server-side floor 0.30."
            ),
            "the window note must say the floor took nothing; got: {fragment}"
        );
    }

    /// The measured bands from `docs/configuration.md`, so a bare `0.31` in the table means
    /// something. Model-dependent and measured, and the fragment has to say both.
    #[tokio::test]
    async fn test_query_tester_prints_the_measured_score_bands() {
        let server = super::super::test_support::banded_server().await;
        seed_straddling_pair(&server).await;
        let app = crate::debug::router(server);

        let fragment = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&dry_run=true",
        )
        .await;
        for needle in [
            super::SCORE_BANDS_MODEL,
            "measured 2026-09-08",
            "model-dependent",
        ]
        .iter()
        .chain(super::SCORE_BAND_RANGES.iter())
        {
            assert!(
                fragment.contains(needle),
                "the score-band legend must state {needle:?}; got: {fragment}"
            );
        }
        // The legend is one const, so the fragment must carry it whole — a partial render (a lost
        // clause, a truncated string continuation) would still satisfy the per-band needles above.
        assert!(
            fragment.contains(&format!(
                "Similarity bands for <code>{}</code>, {}",
                super::SCORE_BANDS_MODEL,
                super::SCORE_BANDS_LEGEND
            )),
            "the rendered legend must be the const verbatim; got: {fragment}"
        );
    }

    /// Guards the claim that [`super::SCORE_BAND_RANGES`] is an index into the legend rather than a
    /// second copy of it: every listed range must occur in the sentence the UI renders.
    #[test]
    fn score_band_ranges_are_in_the_legend() {
        for range in super::SCORE_BAND_RANGES {
            assert!(
                super::SCORE_BANDS_LEGEND.contains(range),
                "{range:?} is not in the legend: {:?}",
                super::SCORE_BANDS_LEGEND
            );
        }
    }

    /// `docs/configuration.md` is the operator-facing contract for these same numbers: it is where
    /// someone tunes `retrieve.min_similarity` looking for what a score *means*. Two independent
    /// copies of one measurement was the bug, and the doc is not compiled, so the only way to make
    /// the pairing a build-time fact is to read the file in at compile time and assert against it
    /// here — an `include_str!` of markdown, not a runtime read, so a moved/renamed doc is a compile
    /// error rather than a silently vacuous pass.
    #[test]
    fn configuration_md_quotes_every_score_band() {
        const CONFIG_MD: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/configuration.md"
        ));
        // The doc uses en-dashes in the ranges (`0.55–0.76`); the template cannot, because askama
        // would happily carry a non-ASCII dash into a `<code>`-adjacent run. Normalize so the
        // comparison is about the *numbers*, not the typography.
        let doc = CONFIG_MD.replace('\u{2013}', "-");
        for needle in [super::SCORE_BANDS_MODEL, "measured 2026-09-08"]
            .iter()
            .chain(super::SCORE_BAND_RANGES.iter())
        {
            assert!(
                doc.contains(needle),
                "docs/configuration.md must keep quoting {needle:?} from SCORE_BANDS_LEGEND"
            );
        }
        // The doc's table cell opens a sentence with "Model-dependent:" where the UI says
        // "model-dependent" mid-sentence — the same claim in a different case, so this one needle
        // is compared without case.
        assert!(
            doc.to_ascii_lowercase().contains("model-dependent"),
            "docs/configuration.md must keep calling these bands model-dependent"
        );
    }

    /// A wet run must say that activation ran and name the ids it seeded — the kept ones, never
    /// a row the floor had already dropped.
    #[tokio::test]
    async fn test_query_tester_reports_activation_seeds_on_a_wet_run() {
        let server = super::super::test_support::banded_server().await;
        let (strong_id, weak_id) = seed_straddling_pair(&server).await;
        let app = crate::debug::router(server);

        let fragment = run_form(app, "mode=retrieve&query=project+decision&limit=10").await;
        assert!(
            fragment.contains(&format!(
                "Spreading activation ran, seeded from 1 of 1 kept results: {strong_id}."
            )),
            "a wet run must name the ids activation was seeded from; got: {fragment}"
        );
        assert!(
            !fragment.contains(&format!("kept results: {weak_id}")),
            "a row the floor dropped must not be reported as an activation seed; got: {fragment}"
        );
    }

    /// The dry half of the same claim.
    #[tokio::test]
    async fn test_query_tester_reports_no_activation_on_a_dry_run() {
        let server = super::super::test_support::banded_server().await;
        seed_straddling_pair(&server).await;
        let app = crate::debug::router(server);

        let fragment = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&dry_run=true",
        )
        .await;
        assert!(
            fragment.contains("Spreading activation did not run — this retrieval wrote nothing."),
            "a dry run must say it did not activate; got: {fragment}"
        );
        assert!(
            !fragment.contains("Spreading activation ran"),
            "a dry run must not claim it activated; got: {fragment}"
        );
    }

    /// The unfiltered window path is what every tester run now reads its dropped rows from, so it
    /// must stay non-mutating. The seeded pair is joined by an edge specifically: with no edge,
    /// activation would find nothing to warm and this assertion would pass even if the path *did*
    /// call `trigger_activation`.
    #[tokio::test]
    async fn test_query_tester_unfiltered_window_writes_no_heat() {
        let server = super::super::test_support::banded_server().await;
        let ids = seed_banded_edge_pair(&server).await;
        let before = banded_heat(&server, &ids).await;
        assert_eq!(
            before,
            (Some(1.0), Some(1.0)),
            "each seeded memory starts at heat 1.0"
        );

        let app = crate::debug::router(server.clone());
        let fragment = run_form(
            app,
            "mode=retrieve&query=project+decision&limit=10&dry_run=true",
        )
        .await;
        assert!(
            fragment.contains("a strong match memory one")
                && fragment.contains("a strong match memory two"),
            "the run must actually retrieve the seeded pair; got: {fragment}"
        );
        assert_eq!(
            banded_heat(&server, &ids).await,
            before,
            "the dry run's unfiltered window must leave heat exactly where it was"
        );
    }

    /// Two memories above the floor — so the window the tester reads is never empty — joined by a
    /// real `memory_edge`, which is what spreading activation walks.
    async fn seed_banded_edge_pair(server: &crate::AlexandriaServer) -> (String, String) {
        let edge_repo = alexandria_storage::repos::EdgeRepo::new(server.db.inner());
        let mut ids = Vec::new();
        for content in ["a strong match memory one", "a strong match memory two"] {
            ids.push(
                server
                    .do_store_memory(crate::tools::StoreMemoryParams {
                        content: content.to_string(),
                        tags: None,
                        session_id: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        edge_repo
            .create_edge(&ids[0], &ids[1], "relates_to", 1.0)
            .await
            .unwrap();
        (ids[0].clone(), ids[1].clone())
    }

    /// Heat of both seeded memories, read through `HeatRepo` — no raw query from a route test.
    async fn banded_heat(
        server: &crate::AlexandriaServer,
        ids: &(String, String),
    ) -> (Option<f64>, Option<f64>) {
        let heat_repo = alexandria_storage::repos::HeatRepo::new(server.db.inner());
        (
            heat_repo.get(&ids.0).await.unwrap().map(|h| h.heat),
            heat_repo.get(&ids.1).await.unwrap().map(|h| h.heat),
        )
    }

    /// The tool's own filtering must be untouched by the options plumbing: one row in, and it is
    /// the above-floor one. `server.rs`'s boundary tests cover the same property at the engine's
    /// edges; this pins that the *debug* entry point added alongside it did not change the tool.
    #[tokio::test]
    async fn test_unfiltered_window_is_a_superset_of_the_tool_results() {
        let server = super::super::test_support::banded_server().await;
        seed_straddling_pair(&server).await;
        let params = || crate::tools::RetrieveMemoriesParams {
            query: "project decision".to_string(),
            limit: Some(10),
            session_id: None,
        };

        let tool = server.do_retrieve_memories(params()).await.unwrap();
        let dry = server.do_retrieve_memories_dry(params()).await.unwrap();
        let window = server
            .do_retrieve_memories_unfiltered(params())
            .await
            .unwrap();
        let rows = |value: &serde_json::Value| {
            value["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        let tool_rows = rows(&tool);
        assert_eq!(
            tool_rows,
            rows(&dry),
            "dry and wet retrieval must return the same rows"
        );
        assert_eq!(
            tool_rows.len(),
            1,
            "the tool must still drop the below-floor memory; got: {tool}"
        );
        let window_rows = rows(&window);
        assert!(
            window_rows.len() > tool_rows.len(),
            "the unfiltered window must show strictly more rows than the tool; got {window_rows:?} \
             vs {tool_rows:?}"
        );
        for id in &tool_rows {
            assert!(
                window_rows.contains(id),
                "every tool result must also be in the window; missing {id}"
            );
        }
    }

    /// Requirement: stop the form lying about the fields recall ignores. The note is a constant —
    /// see [`super::RECALL_IGNORES_NOTE`] — so it has to appear identically whether or not the
    /// retrieve-only fields were filled in. `test_query_run_recall_ignores_dry_run` already pins
    /// that two recall fragments differing only in `dry_run` are byte-identical; this pins the
    /// other half, that a fully filled-in submission says the same thing.
    #[tokio::test]
    async fn test_query_tester_recall_note_is_constant() {
        let server = super::super::test_support::test_server().await;
        server
            .do_store_memory(crate::tools::StoreMemoryParams {
                content: "a recallable fact".to_string(),
                tags: None,
                session_id: None,
            })
            .await
            .unwrap();
        let app = crate::debug::router(server);

        let defaults = run_form(app.clone(), "mode=recall&query=recallable+fact").await;
        let filled = run_form(
            app.clone(),
            "mode=recall&query=recallable+fact&limit=3&session_id=sess-zzz&dry_run=true",
        )
        .await;
        for fragment in [&defaults, &filled] {
            assert!(
                fragment.contains(super::RECALL_IGNORES_NOTE),
                "every recall fragment must name the ignored fields; got: {fragment}"
            );
        }
        // And retrieve mode must not claim it ignores anything.
        let retrieve = run_form(
            app,
            "mode=retrieve&query=recallable+fact&limit=3&dry_run=true",
        )
        .await;
        assert!(
            !retrieve.contains(super::RECALL_IGNORES_NOTE),
            "the recall note must not appear on a retrieve fragment; got: {retrieve}"
        );
    }
}
