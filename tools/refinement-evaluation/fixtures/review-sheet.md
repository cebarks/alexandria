# Refinement evaluation — review sheet

This is the human-readable rendering of `cases.jsonl`, kept in sync with it. It shows each case's
full proposed disposition (including the evidence/hedge language the proposer actually wrote, not
a shortened paraphrase), its tags, and the context the proposer could see, so a reviewer does not
need to read raw JSON to see everything the proposal was built on. See `README.md` (same
directory) for the full schema.

Synthetic data only — no real memories, projects or users appear below.

**Review status: all nine cases below were reviewed and accepted by the project owner in a single
batch, 2026-10-17T17:10:00Z, after viewing the complete nine-case preview.** The owner's decision
was `ACCEPT` on every row, as a batch, not nine independent per-row sign-offs — that is recorded
literally below instead of nine separately-dated entries, because that is not what happened. For
any future case added to this sheet (via a correction or rejection pass, or a new case), use:
either write `ACCEPT` next to the proposed action, `CORRECT: <action> — <reason>` with a
replacement disposition, `REJECT: <reason>` to exclude the case from ground truth without deleting
it, or leave the line blank to skip the case for now (blank stays `proposed`/`pending_human_review`
in `cases.jsonl`, same as an untouched case).

---

## REF-EVAL-001 — error_lesson_vs_receipt (project: widget-api)

- m1 (tags: widget-cli, ci, deploy): "Running `widget-cli deploy --env staging` without `--yes` hangs waiting for a TTY prompt in CI; always pass `--yes` in non-interactive scripts."
  - Context the proposer saw: Stored after an agent debugged a stuck CI job for ~20 minutes and found the root cause.
- m2 (tags: widget-cli, deploy): "Ran `widget-cli deploy --env staging --yes` at 14:02 UTC, exit code 0."
  - Context the proposer saw: Stored immediately after a routine successful deploy command, no debugging involved.

**Proposed:** `no_op` — m1 is a durable, generalizable lesson (a flag that prevents a reproducible failure mode) and is worth keeping as-is. m2 is a receipt of one specific invocation with no generalizable content beyond 'it ran'. They are not duplicates or contradictions of each other, and m2 is not inherently low-value, but it is a much weaker retrieval candidate than m1. The conservative proposal is to leave both alone and flag m2 as a lower-confidence noise/quarantine candidate for a human to confirm, rather than auto-classifying it as noise.

Evidence limits: The proposer sees only the two memory texts and their stored_context strings. It cannot see whether m2's deploy was later referenced, linked to an incident, or otherwise given value by hindsight, so it abstains from unilaterally quarantining m2.

**Accepted outcome (owner, batch, 2026-10-17):** `no_op` — both records are kept unchanged. The noise/quarantine flag on m2 above is informational only; accepting `no_op` does not grant quarantine authority over m2, and this review did not separately decide whether to pursue that flag.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-002 — similar_but_distinct (project: widget-api)

- m1 (tags: rate-limit, api-key): "The widget-api rate limiter uses a token bucket with a 100 req/min ceiling per API key."
  - Context the proposer saw: Stored while documenting the public-facing API gateway's per-key limiter.
- m2 (tags: rate-limit, service-account, admin): "The widget-api internal admin endpoints use a token bucket with a 1000 req/min ceiling per service account."
  - Context the proposer saw: Stored while documenting the separate internal admin API's limiter.

**Proposed:** `no_op` — Both describe a token-bucket rate limiter in the same system, which makes them vector-similar, but they govern different endpoints, different principal types (API key vs service account) and different ceilings (100 vs 1000 req/min). Consolidating them would silently drop the distinction the joint design's 'similar-but-distinct' category exists to catch. Conservative proposal is to leave both as separate memories.

Evidence limits: The proposer can see both memory texts and their tags/contexts, which already make the distinguishing detail (endpoint type) explicit; no further evidence is needed to abstain from consolidation here.

**Accepted outcome (owner, batch, 2026-10-17):** Owner selected "Accept all nine" after viewing the complete nine-case preview in interactive human review; no per-case discrepancy was raised for this case.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-003 — version_environment_qualifier (project: widget-api)

- m1 (tags: widget-cli, auth): "`widget-cli login` opens a browser-based OAuth flow automatically."
  - Context the proposer saw: Stored while using widget-cli v2.3.x on a desktop Linux workstation.
- m2 (tags: widget-cli, auth, headless): "`widget-cli login` prints a device code and a URL to visit manually; it does not open a browser."
  - Context the proposer saw: Stored while using widget-cli v3.0.x inside a headless container with no browser.

**Proposed:** `no_op` — The two memories appear contradictory on the surface (opens a browser vs does not) but the stored_context carries version and environment qualifiers (v2.3.x desktop vs v3.0.x headless) that plausibly explain the difference as two true, scoped facts rather than one wrong fact. The conservative proposal is to leave both memories as-is rather than merge or quarantine either, and to flag for a human reviewer whether the content itself should be edited to make the version/environment qualifier explicit in the text (not just stored_context), since recall ranking will not surface stored_context to a reader.

Evidence limits: The proposer only has the two memory texts and their stored_context strings; it cannot confirm against the real widget-cli changelog whether v3.0.x genuinely changed this behavior for every environment, so it does not assert one memory supersedes the other.

**Accepted outcome (owner, batch, 2026-10-17):** `no_op` — no rewrite in this fixture. The supplied version/environment qualifiers remain context as recorded; they do not imply a contradiction or supersession. The proposal's optional content-edit question (above) is an open question the proposer raised, not a hidden instruction to mutate — it was not separately answered by this review.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-004 — unresolved_contradiction (personal)

- m1 (tags: style, indentation): "The user prefers tabs for indentation in all personal projects."
  - Context the proposer saw: Stored from an explicit statement in a personal scripting project six months ago.
- m2 (tags: style, indentation): "The user prefers spaces for indentation."
  - Context the proposer saw: Stored from an explicit statement in a different personal project three weeks ago, with no mention of the earlier tabs preference or any project-specific scoping.

**Proposed:** `abstain` — Both statements are explicit, generalized (not qualified to a specific project or language) and directly contradictory, with no evidence either supersedes the other (no reference to changing their mind, no version/environment qualifier, no project-specific scoping language). Per the joint design, unresolved contradictions require a human; this is exactly that case, not a candidate for automatic resolution or silent merge.

Evidence limits: The proposer has only the two memory texts, tags, and approximate recency (six months vs three weeks ago). It cannot ask the user which currently holds, so it proposes flagging both for human review rather than guessing that the more recent one wins.

**Accepted outcome (owner, batch, 2026-10-17):** `abstain` — the system takes no action and the contradiction is escalated to a human, as proposed. Accepting this outcome does not settle which of tabs or spaces the user actually prefers; that substantive question remains open and unanswered by this review.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-005 — explicit_supersession (project: widget-api)

- m1 (tags: database, staging, config): "The staging database connection string uses host `db-staging-01.internal`."
  - Context the proposer saw: Stored when the staging environment was first provisioned.
- m2 (tags: database, staging, config, correction): "Correction: the staging database was migrated to a new host, `db-staging-02.internal`. The old host `db-staging-01.internal` is permanently decommissioned; do not use the old connection string from here on."
  - Context the proposer saw: Stored two weeks later, after the migration, explicitly flagged by the user as a correction superseding the earlier note.

**Proposed:** `supersede` — m2 explicitly names itself a correction, explicitly references the specific fact in m1 it replaces, and explicitly states the old value is permanently invalid. This matches the 'explicit supersession' category: the newer memory carries unambiguous self-declared supersession language rather than just topical overlap. Conservative proposal: mark m1 as superseded by m2, retaining m1 (no deletion) per the joint design's 'original content is retained' rule, with recall preferring m2.

Evidence limits: The proposer sees only the two memory texts; it cannot independently verify the database was actually migrated, so the proposed action rests entirely on the explicit self-declared supersession language in m2's text, which the joint design treats as sufficient evidence for this category.

**Accepted outcome (owner, batch, 2026-10-17):** Owner selected "Accept all nine" after viewing the complete nine-case preview in interactive human review; no per-case discrepancy was raised for this case.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-006 — fragmentary_preference (personal)

- m1 (tags: style): "Don't use emoji."
  - Context the proposer saw: Stored from a terse one-line correction mid-conversation, with no surrounding context captured about which outputs or situations this applies to.

**Proposed:** `rewrite` — The statement is a real, usable preference but is fragmentary: it lacks scope (which contexts: chat replies, code comments, commit messages, all of the above?) and lacks the triggering context that would let a reader apply it confidently. The conservative proposal is a rewrite that preserves the literal instruction but flags the missing scope as an open question for human confirmation, rather than inventing a scope the proposer cannot verify (e.g. assuming it means 'nowhere, ever' when it might have been said about one specific document).

Evidence limits: The proposer has only the single terse sentence and no surrounding conversation turns, so it cannot recover the original scope and must not guess one; it proposes widening the applied scope only after a human confirms intent.

**Accepted outcome (owner, batch, 2026-10-17):** `rewrite` — the literal instruction ("Don't use emoji.") is preserved and its scope stays explicitly unknown, exactly as proposed. The owner's preview was explicit that no scope should be invented. The proposal's original condition (confirm intent before *widening* scope) is about future scope changes, not about accepting this scope-preserving rewrite; no scope confirmation occurred or is implied here.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-007 — overloaded_memory (project: widget-api)

- m1 (tags: database, deploy, kubernetes, env-vars, on-call): "The widget-api project uses PostgreSQL 15 with a read replica for analytics queries, deploys via GitHub Actions to a Kubernetes cluster on the staging and prod namespaces, requires `WIDGET_API_SECRET` and `WIDGET_DB_URL` environment variables, and the on-call rotation is handled through a separate PagerDuty schedule not tracked here."
  - Context the proposer saw: Stored as one large note combining infra facts captured during a single onboarding conversation, several distinct topics bundled into one memory.

**Proposed:** `split` — This single memory bundles at least three independently retrievable facts (database/replica topology, deploy pipeline/cluster, required env vars) plus one note that is explicitly out of scope (on-call rotation lives elsewhere). Bundling them into one memory means a query about environment variables alone will retrieve unrelated Kubernetes/PostgreSQL detail as noise, and vice versa. Conservative proposal: split into separate memories along the natural fact boundaries already visible in the sentence structure, retaining the original as the generative source per the joint design's split operation.

Evidence limits: The proposer can see the full memory text and can identify topic boundaries from its own clausal structure, but cannot verify whether the database and deploy facts are actually used together by downstream retrieval in a way that would make splitting counterproductive; it proposes the split as a candidate for human confirmation rather than an automatic application.

**Accepted outcome (owner, batch, 2026-10-17):** `split` — every source claim from m1 is preserved across the split, including the on-call/PagerDuty statement ("the on-call rotation is handled through a separate PagerDuty schedule not tracked here"), consistent with source preservation and the owner's preview. No content is dropped by accepting this outcome.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-008 — duplicate_consolidate (project: widget-api)

- m1 (tags: ci, runners): "CI builds for widget-api run on self-hosted runners tagged `widget-ci-linux`."
  - Context the proposer saw: Stored by an agent during a CI configuration review.
- m2 (tags: ci, runners, infra): "CI builds for the widget-api project run on self-hosted runners tagged `widget-ci-linux`."
  - Context the proposer saw: Stored a week later by the same agent re-confirming the same fact in a slightly different session, with an extra `infra` tag and 'the' / 'project' wording added but no new content.

**Proposed:** `consolidate` — The two memories assert the identical fact with only cosmetic wording and tag differences (near-duplicate, not byte-identical, so this is dedup/consolidation rather than literal collapse). Consolidating into one memory with the union of tags preserves retrievability while removing redundant storage. This is conservative because no semantic content is lost: the union of {ci, runners} and {ci, runners, infra} loses nothing by merging.

Evidence limits: The proposer compared only the two given texts and tags; it did not check whether m2's extra 'infra' tag was deliberately added to make the memory discoverable from a different query angle, so it proposes keeping the union of tags rather than silently dropping either tag set.

**Accepted outcome (owner, batch, 2026-10-17):** Owner selected "Accept all nine" after viewing the complete nine-case preview in interactive human review; no per-case discrepancy was raised for this case.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## REF-EVAL-009 — noise_quarantine (personal)

- m1 (tags: none): "ok"
  - Context the proposer saw: Stored from a one-word conversational acknowledgment with no surrounding context captured; the auto-store heuristic treated it as a standalone statement.

**Proposed:** `quarantine` — The content carries no retrievable semantic information on its own: a bare acknowledgment with no subject, no referenced fact, and no tags. This matches the joint design's noise/quarantine category. Quarantine (not deletion) is conservative because the original design requires retaining original content; a human may later discover surrounding context that makes it meaningful, or may confirm it is safe to leave quarantined indefinitely.

Evidence limits: The proposer has only the single word and an empty tag set; it cannot recover the conversation turn that produced it, so it does not guess at intended meaning and proposes quarantine rather than silent deletion or a fabricated interpretation.

**Accepted outcome (owner, batch, 2026-10-17):** Owner selected "Accept all nine" after viewing the complete nine-case preview in interactive human review; no per-case discrepancy was raised for this case.

Owner decision: **ACCEPT (batch, 2026-10-17)**

---

## After review

For every row above that gets an owner decision, update the matching object in `cases.jsonl`:
set `label_status` to `"accepted"`, `"corrected"`, or `"rejected"` (not `"proposed"`), and fill
in `human_review.reviewer`, `human_review.decision` (`"accept"` / `"correct"` / `"reject"`),
`human_review.corrected_disposition` (required if `"correct"`, otherwise `null`), and
`human_review.reviewed_at` (ISO-8601 timestamp). A `"reject"` decision excludes the case from
scored ground truth but keeps it in the file unmodified — never delete a case. Rows left blank
above stay `"proposed"`/`"pending_human_review"` with `human_review` fields left `null`, and are
not usable as ground truth until a later pass. See `README.md`'s `label_status` table for the full
enum and the ground-truth rule.
