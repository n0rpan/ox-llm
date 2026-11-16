"""Local request-budget forwarding and metadata; synthetic files and fake loopback engine only."""
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from oxc.runtime import ctl_models, oxe_runtime
from openai_api_stack import FakeEngine, Stack, ORNITH
from test_openai_conformance import check


class ThinkingBudget(unittest.TestCase):
    def test_bad_budget_is_rejected_before_any_backend_request(self):
        engine = FakeEngine()
        self.addCleanup(engine.close)
        with Stack(engine.port) as stack:
            for value in (-1, 1.5, True, '9'):
                with self.subTest(value=value):
                    before = len(engine.requests)
                    code, _, body = check.call(stack.base, 'POST', '/v1/chat/completions', {
                        'model': 'local-default', 'messages': [{'role': 'user', 'content': 'hi'}],
                        'thinking_budget': value})
                    self.assertEqual(code, 400)
                    self.assertEqual(body['error']['param'], 'thinking_budget')
                    self.assertEqual(len(engine.requests), before)

    def test_old_engine_budget_is_refused_before_admission(self):
        engine = FakeEngine()
        self.addCleanup(engine.close)
        with Stack(engine.port) as stack:
            code, _, body = check.call(stack.base, 'POST', '/v1/chat/completions', {
                'model': 'local-default', 'messages': [{'role': 'user', 'content': 'hi'}],
                'thinking_budget': 3})
            self.assertEqual(code, 400)
            self.assertEqual(body['error']['param'], 'thinking_budget')
            self.assertFalse(engine.requests)
            self.assertEqual(stack.manager.loads, 0)

    def test_budget_and_explicit_thinking_reach_the_real_proxy_backend_unchanged(self):
        engine = FakeEngine()
        self.addCleanup(engine.close)
        with Stack(engine.port) as stack:
            marker = stack.tmp / 'synthetic-engine'
            marker.write_bytes(b'oxe-capability thinking-budget-v1')
            cfg = json.loads(stack.config_path.read_text())
            cfg['profiles'][ORNITH]['server_binary'] = str(marker)
            stack.set_config(profiles=cfg['profiles'])
            for budget in (None, 0, 37):
                code, _, _ = check.call(stack.base, 'POST', '/v1/chat/completions', {
                    'model': 'local-default', 'messages': [{'role': 'user', 'content': 'hi'}],
                    'thinking_budget': budget, 'chat_template_kwargs': {'enable_thinking': True}})
                self.assertEqual(code, 200)
                sent = engine.requests[-1]['body']
                self.assertEqual(sent['thinking_budget'], budget)
                self.assertTrue(sent['chat_template_kwargs']['enable_thinking'])

    def test_nonchat_budget_is_not_silently_dropped_by_a_translation(self):
        engine = FakeEngine()
        self.addCleanup(engine.close)
        with Stack(engine.port) as stack:
            for route, field in (('/v1/responses', 'input'), ('/v1/completions', 'prompt')):
                code, _, _ = check.call(stack.base, 'POST', route, {
                    'model': 'local-default', field: 'hi', 'thinking_budget': 3})
                self.assertEqual(code, 400)
                self.assertFalse(engine.requests)

    def test_capability_probe_is_cached_by_binary_identity_and_unknown_is_false(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'synthetic-engine'
            binary.write_bytes(b'old engine')
            self.assertFalse(oxe_runtime.engine_has_thinking_budget(binary))
            binary.write_bytes(b'new engine oxe-capability thinking-budget-v1')
            self.assertTrue(oxe_runtime.engine_has_thinking_budget(binary))
            binary.unlink()
            self.assertFalse(oxe_runtime.engine_has_thinking_budget(binary))

    def test_capability_is_local_and_opt_in_no_configured_default_changes(self):
        with patch.object(ctl_models, 'engine_family', return_value='rust'), \
             patch.object(oxe_runtime, 'engine_has_thinking_budget', return_value=True):
            caps = ctl_models.capabilities({'server_binary': '/synthetic/engine'}, {'cap': 8192})
            self.assertTrue(caps['thinking_budget'])
        body = {'messages': [{'role': 'user', 'content': 'hi'}]}
        result = oxe_runtime.request_defaults(body, '/v1/chat/completions', {})
        self.assertNotIn('thinking_budget', result)


from test_cloud_remote import CloudCase
from test_lan_share import ShareBase, CHAT


class CloudBudget(CloudCase):
    def test_null_budget_is_disabled_and_never_sent_upstream(self):
        self.start()
        code, body = self.chat(thinking_budget=None)
        self.assertEqual(code, 200, body)
        self.assertNotIn('thinking_budget', self.upstream.requests[-1]['body'])

    def test_local_budget_is_refused_without_spending_at_an_upstream(self):
        self.start()
        for stream in (False, True):
            code, body = self.chat(stream=stream, thinking_budget=64)
            self.assertEqual(code, 400, body)
            self.assertFalse(self.upstream.requests)


class SharedBudget(ShareBase):
    def test_shared_request_refuses_local_budget_before_admission(self):
        code, _ = self.send('POST', CHAT, self.chat_body(thinking_budget=3), key=self.key)
        self.assertEqual(code, 400)
        self.manager.begin_queued.assert_not_called()
        self.assertFalse(self.bodies)
