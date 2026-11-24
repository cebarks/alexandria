# Refinement evaluation and review fixture

`just refinement-eval-test` runs only the dependency-free Python tests.
`just refinement-eval` scores the deterministic proposal sample against Task 1's
owner-reviewed [`fixtures/cases.jsonl`](fixtures/cases.jsonl). The case set and
[`fixtures/review-sheet.md`](fixtures/review-sheet.md) are the source of truth;
`deterministic-proposals.jsonl` contains model-output stand-ins, not labels.

The offline scorer accepts separate JSONL case and proposal paths. A proposal
names `case_id`, `workspace_scope`, and `action`; include `replacements` (an array
of proposed replacement texts) to check the version/environment case for
omitted literals. The evaluator never writes either input or approves labels.
An explicit `abstain` matching the reviewed disposition is correct; an absent
proposal or a safe abstention on a different labeled action is counted as
abstained. A scope mismatch is incorrect. A mutation where the human label
requires `abstain` is an unsupported resolution. Unusable labels and unknown
proposal IDs are reported separately and excluded from scored counts. Duplicate
IDs are rejected. Qualification flags report missing literals, not whether the
replacement preserves each qualifier's relationship to its claim.

Do not run a model evaluation by default. If approved, use only the approved
endpoint and fixtures, score its outputs separately, and report per-category
counts with concrete false positives and qualification losses. Do not infer a
universal score from nine cases or use model agreement as a human label. Manual
review remains the default regardless of results.

## Browser review fixture

`just review-fixture` is exactly
`cargo run --locked -p alexandria-mcp --example review_fixture`. The first three
stdout lines are the readiness contract, emitted once the fixture is seeded and
the loopback listener has bound:

- `REVIEW_FIXTURE_URL=http://127.0.0.1:<ephemeral-port>/debug/login`
- `REVIEW_FIXTURE_LOGIN=<fixture account>`
- `REVIEW_FIXTURE_PASSWORD=<fresh process-local password>`

Open only that printed URL and use the generated fixture credentials. Keep the
password out of saved screenshots and audit artifacts. The fixture does not
read Alexandria configuration, attach to an existing store, or perform a live
migration or cutover. Stop the owned process when the review is finished.

After login, open **Refinement** and use the seeded, labeled proposals for
approve/rollback, dismiss, defer, and stale-review checks. For a stale review,
keep the **move while review is open** approval form open, move its source from
**Collections** in a second tab, then submit the old form. Use
**Classification** to file the seeded quarantined Unassigned note and verify it
stays quarantined. Compare the proposal state and **Operations** detail rather
than relying on screenshots alone.

The optional `REVIEW_FIXTURE_OPINION_URL` and `REVIEW_FIXTURE_REGENERATE_URL`
lines point to owner-scoped attempt pages for completed deterministic worker
turns. The production browser currently has no opinion/regenerate submission
UI and its enqueue route is unavailable; the fixture does not add a bypass.
Inspect attempt statuses and find the regenerated proposal in the queue.
Browser interaction and saved evidence are separate controller-owned steps.
