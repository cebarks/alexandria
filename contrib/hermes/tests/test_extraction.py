"""Extraction failure behavior is hermetic; these fixtures do not score model quality."""
import unittest
from types import SimpleNamespace
from unittest.mock import Mock

from _bootstrap import bootstrap_hermes
bootstrap_hermes()
from alexandria.extraction import run_extraction, parse_extraction_response


class ExtractionContracts(unittest.TestCase):
    messages = [{'role': 'user', 'content': 'We deploy production with just deploy, never manually. ' * 8}]

    def llm(self):
        llm = Mock()
        llm.complete_structured.side_effect = RuntimeError('structured unavailable')
        llm.complete.return_value = SimpleNamespace(text='{"memories":[{"content":"Use just deploy","tags":["infra"]}]}')
        return llm

    def test_structured_failure_falls_back_to_plain(self):
        llm = self.llm()
        self.assertEqual(run_extraction(self.messages, [], llm=llm)[0]['content'], 'Use just deploy')
        llm.complete.assert_called_once()

    def test_both_lanes_fail_open(self):
        llm = self.llm()
        llm.complete.side_effect = TimeoutError('deadline')
        self.assertEqual(run_extraction(self.messages, [], llm=llm), [])

    def test_malformed_model_output_is_not_a_fact(self):
        llm = self.llm()
        llm.complete.return_value = SimpleNamespace(text='not json')
        self.assertEqual(run_extraction(self.messages, [], llm=llm), [])
        self.assertEqual(parse_extraction_response('{"memories":[{"content":null}]}'), [])

    def test_operation_timeout_is_forwarded_to_both_lanes(self):
        llm = self.llm()
        run_extraction(self.messages, [], llm=llm, timeout=0.25)
        self.assertEqual(llm.complete_structured.call_args.kwargs['timeout'], 0.25)
        self.assertEqual(llm.complete.call_args.kwargs['timeout'], 0.25)
