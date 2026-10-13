import json
import math
import tempfile
import unittest
from pathlib import Path
from pilot_metrics import evaluate_query, read_vectors, validate_questions

class PilotTests(unittest.TestCase):
    def test_historical_targets_absent_from_a_freeze_are_rejected_before_inference(self):
        records = [{'id':'present','content':'public'}]
        validate_questions(records, [{'query':'public question','target':'present'}])
        with self.assertRaises(ValueError):
            validate_questions(records, [{'query':'historical question','target':'missing'}])

    def test_multiple_relevant_labels_and_inclusive_paired_thresholds(self):
        result = evaluate_query({'a': .9, 'b': .5, 'c': .4}, {'a', 'b'}, 3, .1, .5)
        self.assertEqual(result['returned'], 2)
        self.assertEqual(result['precision_bounds'], [1, 1])
        self.assertEqual(result['recall_bounds'], [1, 1])
        self.assertEqual(result['rr_bounds'], [1, 1])
        self.assertFalse(result['abstained'])
        self.assertEqual(evaluate_query({'a': .9, 'b': .5}, {'a', 'b'}, 2, .6, .1)['returned'], 1)

    def test_tie_boundary_cannot_fabricate_a_unique_top_one(self):
        result = evaluate_query({'negative': .8, 'relevant': .8, 'other-relevant': .8}, {'relevant', 'other-relevant'}, 1, .1, .1)
        self.assertEqual(result['precision_bounds'], [0, 1])
        self.assertEqual(result['recall_bounds'], [0, .5])
        self.assertEqual(result['rr_bounds'], [0, 1])
        self.assertFalse(result['unique_top1'])
        both = evaluate_query({'negative': .8, 'relevant': .8, 'other-relevant': .8}, {'relevant', 'other-relevant'}, 3, .1, .1)
        self.assertEqual(both['rr_bounds'], [.5, 1])
        self.assertEqual(both['recall_bounds'], [1, 1])

    def test_unanswerable_false_acceptance_and_abstention_are_separate(self):
        result = evaluate_query({'near-miss': .61}, set(), 1, .1, .6)
        self.assertTrue(result['false_accept'])
        self.assertEqual(result['precision_bounds'], [0, 0])
        self.assertIsNone(result['recall_bounds'])
        empty = evaluate_query({'near-miss': .59}, set(), 1, .1, .6)
        self.assertFalse(empty['false_accept'])
        self.assertTrue(empty['abstained'])
        self.assertIsNone(empty['precision_bounds'])

    def test_invalid_scores_labels_limits_and_thresholds_are_not_silently_used(self):
        for scores, labels, limit, server, client in [
            ({'a': math.nan}, set(), 1, .1, .5), ({'a': math.inf}, set(), 1, .1, .5),
            ({'a': 1.5}, set(), 1, .1, .5), ({'a': .5}, {'missing'}, 1, .1, .5),
            ({'a': .5}, set(), 0, .1, .5), ({'a': .5}, set(), True, .1, .5),
            ({'a': .5}, set(), 1, math.nan, .5), ({'a': .5}, set(), 1, .1, 1.1),
        ]:
            with self.assertRaises(ValueError): evaluate_query(scores, labels, limit, server, client)

    def test_partial_duplicate_and_nonunit_native_outputs_cannot_be_success_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'vectors.jsonl'
            start = {'type':'start','format_version':1,'sample_count':1,'input_sha256':'a'*64}
            ready = {'type':'ready','space':{'kind':'legacy','model':'public','dimensions':2,'max_tokens':128},'device':'cpu'}
            row = {'type':'vector','id':'x','vector':[1.,0.]}
            complete = {'type':'complete','sample_count':1}
            def put(rows): path.write_text(''.join(json.dumps(r)+'\n' for r in rows))
            put([start,ready,row,complete]); result = read_vectors(path)
            self.assertEqual(result['vectors']['x'], [1.,0.])
            for bad in [[start,ready,row], [start,ready,row,row,complete], [start,ready,{**row,'vector':[2.,0.]},complete], [start,ready,{**row,'vector':[1.]},complete], [start,ready,{**row,'vector':[math.nan,0.]},complete], [{**start,'format_version':True},ready,row,complete], [start,ready,row,{**complete,'sample_count':True}]]:
                put(bad)
                with self.assertRaises(ValueError): read_vectors(path)

if __name__ == '__main__': unittest.main()
