"""Offline checks for fixture isolation, incremental order, and error accounting."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT/'scripts'))
import aml_seven_eval as evaluation


class FakeRunner:
    def __init__(self, *args):
        self.events, self.checks, self.sources, self.requests = [], [], [], []

    def call(self, kind, payload):
        self.requests.append((kind, copy.deepcopy(payload)))
        self.events.append({'kind': kind, 'status': 200, 'elapsed_ms': 1})
        self.checks.append({'passed': True})
        if kind == 'add':
            self.sources.extend(payload['messages'])
            return 200, {'success': True}
        return 200, {'data': [{'id': str(i), 'content': s['content']}
                             for i, s in enumerate(self.sources)]}


class SevenEvalTests(unittest.TestCase):
    def setUp(self):
        self.fixture = json.loads((ROOT/'tests/fixtures/aml-seven-v1.json').read_text())

    def test_frozen_split_and_wire_boundary(self):
        self.assertEqual(evaluation.validate_fixture(self.fixture)['development_checkpoints'], 48)
        for case in self.fixture['cases']:
            sources = evaluation.noisy_sources(case)
            self.assertEqual(len(sources), case['noise_count']+len(case['initial_sources']))
            self.assertEqual(len({s['id'] for s in sources}), len(sources))
            r = FakeRunner()
            evaluation.ingest(r, 'opaque:user', 'opaque:batch', sources)
            for kind, payload in r.requests:
                self.assertEqual(kind, 'add')
                self.assertEqual(set(payload), {'request_id', 'user_id', 'session_id', 'messages'})
                self.assertLessEqual(len(payload['messages']), 20)
                for message in payload['messages']:
                    self.assertEqual(set(message), {'role', 'content', 'timestamp'})
                    self.assertIs(type(message['timestamp']), int)
            for point in case['checkpoints']:
                self.assertEqual(set(evaluation.answer_payload(point, [])), {'question', 'options', 'evidence'})

    def test_incremental_answer_and_judge_have_no_future(self):
        case = next(c for c in self.fixture['cases'] if c['id'] == 'governance-01')
        calls = []

        def fake_model(base, key, model, prompt, payload, judge=False):
            calls.append((judge, copy.deepcopy(payload)))
            if judge:
                return {'score': 1, 'reason': 'offline test'}, {'elapsed_ms': 1}
            text = '\n'.join(e['content'] for e in payload['evidence'])
            return ('Green gate' if 'green gate' in text.lower() else 'Blue gate'), {'elapsed_ms': 1}

        args = type('Args', (), {'base_url': 'http://localhost:8100', 'timeout': 1})()
        config = {key: 'placeholder' for key in ('memory_key', 'answer_base', 'answer_key', 'answer_model',
                                               'judge_base', 'judge_key', 'judge_model')}
        with patch.object(evaluation, 'Runner', FakeRunner), patch.object(evaluation, 'model_call', fake_model):
            result = evaluation.execute_case(case, 'offline', args, config)
        self.assertEqual([p['score'] for p in result['results']], [1, 1])
        first_answer, first_judge = calls[0][1], calls[1][1]
        self.assertNotIn('green gate', json.dumps(first_answer).lower())
        self.assertNotIn('green gate', json.dumps(first_judge).lower())
        self.assertEqual(set(first_answer), {'question', 'options', 'evidence'})
        self.assertIn('green gate', json.dumps(calls[2][1]).lower())

    def test_transport_errors_are_unscored(self):
        case = next(c for c in self.fixture['cases'] if c['id'] == 'governance-01')
        args = type('Args', (), {'base_url': 'http://localhost:8100', 'timeout': 1})()
        with patch.object(evaluation, 'ingest', side_effect=RuntimeError('Add failed: HTTP 500')):
            result = evaluation.execute_case(case, 'offline', args, {'memory_key': 'placeholder'})
        self.assertEqual(len(result['results']), 2)
        self.assertTrue(all(not p['scored'] and 'score' not in p for p in result['results']))
        summary = evaluation.aggregate([result], self.fixture, [case])
        self.assertEqual((summary['expected_checkpoints'], summary['scored_checkpoints'],
                          summary['unscored_checkpoints']), (2, 0, 2))

    def test_refusal_with_disclosure_and_wrong_order_fail(self):
        gold = {'required_groups': [['backup'], ['tests'], ['deploy']], 'ordered_groups': True,
                'forbidden_fragments': ['SilverOrchid']}
        self.assertTrue(evaluation.hard_checks('backup → tests → deploy', gold)['passed'])
        self.assertFalse(evaluation.hard_checks('deploy → tests → backup', gold)['passed'])
        self.assertFalse(evaluation.hard_checks('backup tests deploy. I cannot share SilverOrchid.', gold)['passed'])

    def test_valid_paraphrase_is_not_blocked_by_aliases(self):
        gold = {'required_groups': [['unknown']], 'ordered_groups': False, 'forbidden_fragments': []}
        checks = evaluation.hard_checks('No location has been confirmed.', gold)
        self.assertFalse(checks['passed'])
        self.assertTrue(checks['safety_and_order_passed'])

    def test_judge_does_not_receive_aliases_as_exhaustive_gold(self):
        point = self.fixture['cases'][0]['checkpoints'][0]
        payload = evaluation.judge_payload(point, 'Porto', [])
        self.assertNotIn('required_groups', payload['gold'])
        self.assertEqual(payload['answer'], 'Porto')

    def test_rescore_evaluates_every_answer_without_memory_calls(self):
        cases = [c for c in self.fixture['cases'] if c['split'] == 'development']
        report = {'fixture_sha256': evaluation.digest(json.dumps(self.fixture).encode()),
                  'split': 'development', 'models': {'judge': 'offline'}, 'prompts': {},
                  'limitations': [], 'cases': [
                      {'case_id': c['id'], 'events': [], 'contract_checks': [], 'results': [
                          {'case_id': c['id'], 'dimension': c['dimension'], 'checkpoint_id': p['id'],
                           'answer': p['gold']['reference']} for p in c['checkpoints']]} for c in cases]}
        calls = []

        def fake_judge(base, key, model, prompt, payload, judge=False):
            self.assertTrue(judge)
            calls.append(copy.deepcopy(payload))
            return {'score': 1, 'reason': 'offline'}, {'elapsed_ms': 1, 'attempts': 1, 'usage': {}}

        with tempfile.TemporaryDirectory() as folder:
            source, output = Path(folder)/'original.json', Path(folder)/'rescore.json'
            source.write_text(json.dumps(report))
            original = source.read_bytes()
            args = type('Args', (), {'rescore': source, 'output': output, 'split': 'development', 'workers': 2})()
            config = {'judge_model': 'offline', 'judge_base': 'offline', 'judge_key': 'placeholder'}
            with (patch.object(evaluation, 'Runner', side_effect=AssertionError('No memory calls allowed')),
                  patch.object(evaluation, 'model_call', fake_judge), patch('builtins.print')):
                rescored = evaluation.rescore_report(args, config, self.fixture, json.dumps(self.fixture).encode())
            self.assertEqual(source.read_bytes(), original)
        self.assertEqual(len(calls), 48)
        self.assertEqual(rescored['summary']['scored_checkpoints'], 48)


if __name__ == '__main__':
    unittest.main()
