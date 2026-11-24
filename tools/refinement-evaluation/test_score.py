"""Contracts against the Task 1 reviewed cases, not an evaluator-generated label set."""

import copy
import unittest
from pathlib import Path

from score import read_jsonl, score


ROOT = Path(__file__).resolve().parent


class ScoreTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cases = read_jsonl(ROOT / "fixtures" / "cases.jsonl")
        cls.by_id = {case["case_id"]: case for case in cls.cases}

    def test_empty_has_no_categories_or_examples(self):
        self.assertEqual(score([], []), {
            "categories": {},
            "unsupported_resolution": [],
            "qualification_loss": [],
            "unknown_ids": [],
            "excluded_case_ids": [],
        })

    def test_unknown_proposals_cannot_inflate_denominator(self):
        case = self.by_id["REF-EVAL-004"]
        report = score([case], [
            {"case_id": "unknown", "action": "split", "workspace_scope": "personal"},
        ])
        self.assertEqual(report["unknown_ids"], ["unknown"])
        self.assertEqual(report["categories"], {
            "unresolved_contradiction": {"correct": 0, "incorrect": 0, "abstained": 1},
        })
        self.assertEqual(report["unsupported_resolution"], [])

    def test_incomplete_or_inconsistent_human_labels_are_excluded(self):
        missing_timestamp = copy.deepcopy(self.by_id["REF-EVAL-002"])
        missing_timestamp["human_review"]["reviewed_at"] = None
        mismatched_decision = copy.deepcopy(self.by_id["REF-EVAL-004"])
        mismatched_decision["human_review"]["decision"] = "correct"
        missing_correction = copy.deepcopy(self.by_id["REF-EVAL-005"])
        missing_correction["label_status"] = "corrected"
        missing_correction["human_review"]["decision"] = "correct"
        cases = [missing_timestamp, mismatched_decision, missing_correction]
        proposals = [
            {"case_id": case["case_id"], "workspace_scope": case["workspace_scope"],
             "action": case["proposed_disposition"]["action"],
             "human_review": {"reviewer": "synthetic proposal metadata", "decision": "accept"}}
            for case in cases
        ]
        report = score(cases, proposals)
        self.assertEqual(report["excluded_case_ids"], ["REF-EVAL-002", "REF-EVAL-004", "REF-EVAL-005"])
        self.assertEqual(report["categories"], {})
        self.assertEqual(report["unknown_ids"], [])

    def test_human_correction_overrides_original_proposal(self):
        case = copy.deepcopy(self.by_id["REF-EVAL-006"])
        case["label_status"] = "corrected"
        case["human_review"]["decision"] = "correct"
        case["human_review"]["corrected_disposition"] = {"action": "no_op"}
        report = score([case], [
            {"case_id": "REF-EVAL-006", "workspace_scope": "personal", "action": "no_op"},
        ])
        self.assertEqual(report["categories"]["fragmentary_preference"], {
            "correct": 1, "incorrect": 0, "abstained": 0,
        })

    def test_scope_mismatch_is_incorrect_even_when_action_matches(self):
        report = score([self.by_id["REF-EVAL-006"]], [
            {"case_id": "REF-EVAL-006", "workspace_scope": "project:wrong", "action": "rewrite"},
        ])
        self.assertEqual(report["categories"]["fragmentary_preference"]["incorrect"], 1)

    def test_explicit_supported_abstention_is_not_a_missing_proposal(self):
        report = score(self.cases, [
            {"case_id": "REF-EVAL-004", "workspace_scope": "personal", "action": "abstain"},
            {"case_id": "REF-EVAL-006", "workspace_scope": "personal", "action": "abstain"},
        ])
        self.assertEqual(report["categories"]["unresolved_contradiction"]["correct"], 1)
        self.assertEqual(report["categories"]["fragmentary_preference"]["abstained"], 1)
        self.assertEqual(report["categories"]["overloaded_memory"]["abstained"], 1)

    def test_reports_concrete_unsupported_resolution_and_qualification_loss(self):
        proposals = read_jsonl(ROOT / "fixtures" / "deterministic-proposals.jsonl")
        report = score(self.cases, proposals)
        self.assertEqual(len(report["categories"]), 9)
        self.assertEqual(report["categories"]["duplicate_consolidate"]["correct"], 1)
        self.assertEqual(report["categories"]["version_environment_qualifier"]["incorrect"], 1)
        self.assertEqual(report["categories"]["unresolved_contradiction"]["incorrect"], 1)
        self.assertEqual(report["unsupported_resolution"], [{
            "case_id": "REF-EVAL-004", "expected": "abstain", "proposed": "supersede",
        }])
        self.assertEqual([item["case_id"] for item in report["qualification_loss"]], ["REF-EVAL-003"])
        self.assertIn("v3.0.x", report["qualification_loss"][0]["missing_literals"])
        self.assertIn("headless", report["qualification_loss"][0]["missing_literals"])
        self.assertNotIn("v2.3.x", report["qualification_loss"][0]["missing_literals"])


if __name__ == "__main__":
    unittest.main()
