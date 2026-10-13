import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from score_pilot import score_pilot


class ScoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.input, self.vectors, self.labels = [self.root/p for p in ['input.json','vectors.jsonl','labels.json']]
        documents = [{'id':'m','group':'memory','text':'memory public'}, {'id':'c','group':'code','text':'code public'}]
        queries = [{'id':f'{g}/{suffix}', 'group':g, 'text':f'{g} {suffix}', 'relevant_ids':[target] if suffix=='answer' else []}
                   for g,target in [('memory','m'),('code','c')] for suffix in ['answer','negative']]
        self.input.write_text(json.dumps({'format_version':1,'samples':[{'id':r['id'],'text':r['text']} for r in documents+queries]}))
        self.labels.write_text(json.dumps({'format_version':1,'documents':documents,'queries':queries}))
        values = [[1.,0.], [0.,1.], [1.,0.], [-1.,0.], [0.,1.], [0.,-1.]]
        self.rows = [{'type':'start','format_version':1,'sample_count':6,'input_sha256':hashlib.sha256(self.input.read_bytes()).hexdigest()},
                     {'type':'ready','space':{'kind':'legacy','model':'public','dimensions':2,'max_tokens':128},'device':'cpu'}]
        self.rows += [{'type':'vector','id':r['id'],'vector':v} for r,v in zip(documents+queries,values)]
        self.rows += [{'type':'complete','sample_count':6}]
        self.write_vectors()

    def write_vectors(self):
        self.vectors.write_text(''.join(json.dumps(r)+'\n' for r in self.rows))

    def score(self):
        return score_pilot(self.input, self.vectors, self.labels, 3, .1, .5)

    def test_complete_space_and_input_bound_scoring_has_no_false_accepts(self):
        result = self.score()
        for group in ['memory','code']:
            self.assertEqual(result['groups'][group]['recall_bounds'], [1.,1.])
            self.assertEqual(result['groups'][group]['micro_precision_bounds'], [1.,1.])
            self.assertEqual(result['groups'][group]['false_accepts'], 0)
        self.assertEqual(result['space']['model'], 'public')

    def test_roster_hash_text_and_label_tampering_are_rejected(self):
        original = self.input.read_text()
        self.input.write_text(original+' ')
        with self.assertRaises(ValueError): self.score()
        self.input.write_text(original)
        labels = json.loads(self.labels.read_text())
        labels['documents'][0]['text'] = 'different document'
        self.labels.write_text(json.dumps(labels))
        with self.assertRaises(ValueError): self.score()
        labels['documents'][0]['text'] = 'memory public'
        labels['queries'][0]['relevant_ids'] = ['c']
        self.labels.write_text(json.dumps(labels))
        with self.assertRaises(ValueError): self.score()

    def test_cli_output_is_private_and_never_overwrites_failure_evidence(self):
        output = self.root/'metrics.json'
        command = [sys.executable, str(Path(__file__).with_name('score_pilot.py')),
                   '--input', str(self.input), '--vectors', str(self.vectors),
                   '--labels', str(self.labels), '--output', str(output),
                   '--limit', '3', '--server-floor', '.1', '--client-floor', '.5']
        subprocess.run(command, check=True, capture_output=True)
        if os.name == 'posix': self.assertEqual(output.stat().st_mode & 0o777, 0o600)
        output.write_bytes(b'failure evidence')
        self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
        self.assertEqual(output.read_bytes(), b'failure evidence')

    def test_incomplete_vector_stream_is_not_a_scored_success(self):
        self.rows.pop(); self.write_vectors()
        with self.assertRaises(ValueError): self.score()


if __name__ == '__main__': unittest.main()
