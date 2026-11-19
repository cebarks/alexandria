"""Offline scoring of proposals against human-reviewed refinement cases."""

import argparse
import json
import re
from datetime import datetime
from pathlib import Path


CONTENT_ACTIONS = {"rewrite", "consolidate", "supersede", "split"}
MUTATIONS = CONTENT_ACTIONS | {"quarantine"}
ACTIONS = MUTATIONS | {"no_op", "abstain"}
# Only the controlled vocabulary of the Task 1 qualification example, not a semantic judgment.
QUALIFIER = re.compile(r"(?<![\w.])v\d+(?:\.(?:\d+|x))+\b|\b(?:desktop|linux|headless|container)\b", re.I)


def read_jsonl(path):
    with Path(path).open(encoding="utf-8") as source:
        return [json.loads(line) for line in source if line.strip()]


def reviewed_action(case):
    review = case.get("human_review")
    if not isinstance(review, dict):
        return None
    reviewer, timestamp = review.get("reviewer"), review.get("reviewed_at")
    if not isinstance(reviewer, str) or not reviewer.strip() or not isinstance(timestamp, str):
        return None
    try:
        when = datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
        if when.utcoffset() is None:
            return None
    except ValueError:
        return None
    correction = review.get("corrected_disposition")
    status = (case.get("label_status"), review.get("decision"))
    if status == ("accepted", "accept") and correction is None:
        label = case.get("proposed_disposition")
    elif status == ("corrected", "correct"):
        label = correction
    else:
        return None
    action = label.get("action") if isinstance(label, dict) else None
    if (isinstance(action, str) and action in ACTIONS
            and all(isinstance(case.get(key), str) and case[key].strip()
                    for key in ("category", "workspace_scope"))):
        return action
    return None


def index_rows(rows, kind):
    indexed = {}
    for row in rows:
        key = row.get("case_id")
        if not isinstance(key, str) or not key.strip() or key in indexed:
            raise ValueError(f"invalid or duplicate {kind} case_id: {key!r}")
        indexed[key] = row
    return indexed


def missing_qualifiers(case, proposal):
    replacements = proposal.get("replacements")
    if (case["category"] != "version_environment_qualifier"
            or proposal.get("action") not in CONTENT_ACTIONS
            or not isinstance(replacements, list)
            or not all(isinstance(text, str) for text in replacements)):
        return []
    expected = {match.lower() for memory in case.get("memories", [])
                for match in QUALIFIER.findall(memory.get("stored_context", ""))}
    observed = {match.lower() for text in replacements for match in QUALIFIER.findall(text)}
    return sorted(expected - observed)


def score(cases, proposals):
    """Report category counts and evidence examples without altering either input."""
    cases = index_rows(cases, "label")
    proposals = index_rows(proposals, "proposal")
    report = {
        "categories": {},
        "unsupported_resolution": [],
        "qualification_loss": [],
        "unknown_ids": sorted(proposals.keys() - cases.keys()),
        "excluded_case_ids": [],
    }
    for case_id, case in sorted(cases.items()):
        expected = reviewed_action(case)
        if expected is None:
            report["excluded_case_ids"].append(case_id)
            continue
        category = report["categories"].setdefault(case["category"], {
            "correct": 0, "incorrect": 0, "abstained": 0,
        })
        proposal = proposals.get(case_id)
        if proposal is None:
            category["abstained"] += 1
            continue
        action = proposal.get("action")
        if not isinstance(action, str):
            action = ""
        missing = missing_qualifiers(case, proposal) if action in CONTENT_ACTIONS else []
        if proposal.get("workspace_scope") != case["workspace_scope"] or missing:
            category["incorrect"] += 1
        elif action == expected:
            category["correct"] += 1
        elif action == "abstain":
            category["abstained"] += 1
        else:
            category["incorrect"] += 1
        if expected == "abstain" and action in MUTATIONS:
            report["unsupported_resolution"].append({
                "case_id": case_id, "expected": expected, "proposed": action,
            })
        if missing:
            report["qualification_loss"].append({
                "case_id": case_id, "missing_literals": missing,
            })
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cases", type=Path)
    parser.add_argument("proposals", type=Path)
    args = parser.parse_args()
    print(json.dumps(score(read_jsonl(args.cases), read_jsonl(args.proposals)), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
