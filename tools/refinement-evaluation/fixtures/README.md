# Refinement evaluation fixtures

**Status: each case in `cases.jsonl` carries its own review state in `label_status` and
`human_review` (see the schema below) — nothing here is ground truth merely by existing in this
directory.** They were authored by a worker subagent as synthetic, anonymized illustrations of
each category Task 1's brief calls out. No real user corpus was read or copied to produce them. A
model's agreement with a proposed disposition is never a label: every `human_review.reviewer`
value recorded in `cases.jsonl` is a person, and a case stays excluded from ground truth until a
person records `decision`/`reviewer`/`reviewed_at` for it, whatever a model said about the
proposal.

As of the current snapshot, all nine cases have been reviewed and accepted by the project owner
in a single batch pass after viewing the complete case preview (`human_review.reviewer:"project
owner"`, `decision:"accept"`, `reviewed_at:"2026-10-17T17:10:00Z"` on every case). See
`review-sheet.md` for the human-readable record of that review, and the per-field ground-truth
rule below for what a future partial or corrective review pass must set.

## Files

- `cases.jsonl` — one JSON object per line, one evaluation case per line. See schema below.
- `review-sheet.md` — the same cases rendered as a human-readable table/sections, with a
  stable `case_id` per row, meant to be read and annotated by the owner in one pass (accept
  the proposed disposition, or write a correction) without needing to read raw JSON.

## Schema (`cases.jsonl`, one JSON object per line)

```jsonc
{
  "case_id": "REF-EVAL-001",          // stable, stays fixed across edits; used to cross-reference review-sheet.md
  "category": "error_lesson_vs_receipt", // one of the category slugs listed below
  "workspace_scope": "personal",      // "personal" or "project:<synthetic-project-name>"; never a real project name
  "memories": [
    {
      "ref": "m1",                    // case-local reference, not a real record ID
      "content": "...",               // synthetic memory text
      "tags": ["..."],
      "stored_context": "..."         // free-text: when/why this was synthetically 'stored', including
                                       // version/environment qualifiers when the category needs them
    }
    // one or more entries; contradiction/duplicate/supersession/split cases have 2+
  ],
  "proposed_disposition": {
    "action": "no_op | rewrite | consolidate | quarantine | supersede | split | abstain",
    "rationale": "...",               // why this disposition is the conservative proposal; preserved verbatim
                                       // through review — see "Per-case notes vs rationale" below
    "evidence_boundaries": "..."      // what evidence this case's proposer could and could not see;
                                       // states explicitly where the model must abstain rather than guess
  },
  "label_status": "proposed",         // see the full enum below; this field and human_review must agree
  "human_review": {
    "reviewer": null,                 // a PERSON's identity once reviewed; never a model name; null until reviewed
    "decision": null,                 // "accept" | "correct" | "reject"; null until reviewed
    "corrected_disposition": null,    // present only when decision is "correct"; see shape below; null otherwise
    "notes": null,                    // human-written clarification; never invents a disposition or scope the
                                       // proposal itself left open — see "Per-case notes vs rationale" below
    "reviewed_at": null               // ISO-8601 timestamp, or null
  }
}
```

### `label_status` enum and the ground-truth rule

`label_status` is one of:

| Value | Meaning | Matching `human_review` state |
|---|---|---|
| `proposed` / `pending_human_review` | Not yet reviewed by a person. Both spellings mean the same thing and may appear interchangeably in `cases.jsonl`; neither is usable as ground truth. | All five `human_review` fields `null`. |
| `accepted` | A person reviewed the case and accepted `proposed_disposition` as-is. | `reviewer`, `decision:"accept"`, `reviewed_at` set; `corrected_disposition` `null`. |
| `corrected` | A person reviewed the case and replaced the disposition. | `reviewer`, `decision:"correct"`, `reviewed_at`, `corrected_disposition` all set. |
| `rejected` | A person reviewed the case and judged it unusable (mis-categorized, unrealistic, or otherwise not a valid evaluation case). The case **stays in the file**, unmodified and undeleted — rejection is a review outcome, not a deletion — but it is excluded from scored ground truth. | `reviewer`, `decision:"reject"`, `reviewed_at` set; `corrected_disposition` `null`. |

**Ground-truth rule:** a case is usable as ground truth only when `label_status` is `accepted` or
`corrected` *and* `human_review.reviewer`/`decision`/`reviewed_at` are all non-null and consistent
with that status. A `rejected` case is reviewed but intentionally excluded from scoring. A
`proposed`/`pending_human_review` case is not reviewed at all. `label_status` and `human_review`
must always agree; if a future edit updates one without the other, that is a data-integrity bug to
fix before using the file, not a third review state.

### `corrected_disposition` shape

When `human_review.decision` is `"correct"`, `corrected_disposition` mirrors
`proposed_disposition`'s shape: `{"action": "...", "rationale": "...", "evidence_boundaries":
"..."}`, with `action` required and `rationale`/`evidence_boundaries` optional (omit or leave empty
if the person correcting the case does not want to restate them). It is `null` for every other
`decision` value, including `"reject"` — a rejection does not supply a replacement disposition, it
just excludes the case from ground truth.

### Per-case notes vs rationale

`human_review.notes` is written by the reviewing person and may state only outcomes the review
itself actually settled — it must not retroactively rewrite, strengthen, or resolve an open
question that `proposed_disposition.rationale`/`evidence_boundaries` left for a human, unless the
review explicitly addressed that question. `proposed_disposition` fields are never edited by a
review pass; they are the frozen record of what was proposed, not a place to record what was
decided.

### Per-action retention (binding on every action that mutates anything)

None of the seven actions in this schema authorizes permanent deletion of a memory's original
content, matching the source design's "original content is retained" rule. ("The source design"
here means the internal planning document that shaped this fixture set's categories and actions;
it is historical context for why these rules exist, not something a reader of this directory needs
to open — the rules themselves are stated completely below.)

- `no_op` / `abstain` — mutate nothing; these are fixture-level "take no action" outcomes (see
  "`no_op` vs `abstain`" below), not an implicit no-op on retention because there is no mutation to
  retain against.
- `rewrite` — the pre-edit content is retained (soft-deleted/superseded with lineage to the
  rewritten version), not overwritten in place.
- `consolidate` — "removing redundant storage" means the absorbed memory is retained via the same
  soft-delete-plus-lineage mechanism, not hard-deleted; consolidation never discards a memory's
  bytes.
- `supersede` — the superseded memory is retained and marked superseded, not deleted; recall
  prefers the superseding memory.
- `split` — the original is retained as the generative source of the split descendants, not
  removed once split.
- `quarantine` — the memory is retained and hidden from ordinary recall, not deleted; a person can
  reverse quarantine later.

### `no_op` vs `abstain`

Both values mean no mutation is warranted right now — the shared effect is "nothing changes" — but
the tokens are not interchangeable and a scorer comparing `action` values can and should tell them
apart: `rationale`/`evidence_boundaries` is what justifies which token is correct for a given case,
not a tiebreaker needed because the tokens themselves are indistinguishable.

`no_op` means no mutation is warranted now, and it may leave an informational flag unresolved
rather than asserting the current state is fully settled: REF-EVAL-002 fits that cleanly (the two
memories are judged correctly distinct, nothing is left open); REF-EVAL-001 and REF-EVAL-003 are
also `no_op` while each carries a flag the proposal deliberately does not resolve (001: whether m2
is a quarantine candidate; 003: whether the qualifier should move into memory content) — `no_op`
covers both "nothing to resolve" and "an open flag that isn't this pass's job to resolve," and
rationale is what tells a reader which case is which.

`abstain` means the proposer explicitly declines to resolve the case because the evidence is
conflicting or insufficient to choose — most often an unresolved contradiction (REF-EVAL-004) —
without resolving its subject (accepting `abstain` on 004 does not decide tabs vs. spaces).
Unlike `no_op`'s open flags, which are side notes on an otherwise-settled case, `abstain`'s whole
point is that the case itself cannot be settled without a human.

Neither term is one of the six proposal kinds named in the source design (rewrite,
duplicate/consolidate, contradiction, noise/quarantine, supersession, split); both are
fixture-level, legitimately scoreable "no mutation happened" outcomes this evaluation set adds so
every case has a recorded action — not production proposal types, but not degenerate or
interchangeable either.

## Categories (full closed list used by `category` — 9 slugs, 9 cases, one each)

The 7 required by Task 1's brief, verbatim:

1. `error_lesson_vs_receipt` — useful error lessons versus receipts
2. `similar_but_distinct` — similar-but-distinct facts
3. `version_environment_qualifier` — version/environment qualifiers
4. `unresolved_contradiction` — unresolved contradictions
5. `explicit_supersession` — explicit supersession
6. `fragmentary_preference` — fragmentary preferences
7. `overloaded_memory` — overloaded memories

Plus 2 additive categories, drawn from the source design's six proposal kinds (rewrite,
duplicate/consolidate, contradiction, noise/quarantine, supersession, split) so the fixture set is
not missing an example of either — these do not replace or narrow the seven required categories
above, they fill in two of the six proposal kinds that would otherwise have no case:

8. `duplicate_consolidate` — near-duplicate content (not byte-identical) that should be merged
9. `noise_quarantine` — content with no retrievable semantic value that should be quarantined,
   not deleted

`category`'s value in `cases.jsonl` is always one of these 9 slugs; the schema comment above
("one of the category slugs listed below") refers to this full list of 9, not only the 7 brief
categories.

## What this is not

- Not real user data. Every name, project, error message and preference below is invented for
  this fixture set.
- Not the evaluator implementation. Task 23 builds the scorer; this directory only supplies
  labeled inputs for it, usable once `label_status` marks a case `accepted` or `corrected` (see
  the ground-truth rule above).
- Not a claim that the `proposed_disposition` fields are correct merely because a case is now
  accepted. They remain the worker subagent's original, conservative, evidence-bounded proposals
  — the review step judged them acceptable as literal outcomes, it did not rewrite them, and
  `human_review.notes` records only what the review actually decided (see "Per-case notes vs
  rationale" above).
