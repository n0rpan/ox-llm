"""Shared Wave D review controls; isolated loopback mocks and synthetic paths only."""
import json
import sys
from unittest.mock import patch

from oxc.runtime import cloud
from test_cloud_remote import CloudCase, REMOTE, profiles
from test_cloud_anthropic import anthropic_models


class SharedAdmission(CloudCase):
    def test_multiple_choices_refused_before_upstream_under_small_budget(self):
        models = profiles()
        models[REMOTE]['context_window'] = 100
        self.start(budget=0.0015, models=models)
        status, body = self.chat(n=4, max_tokens=1000)
        self.assertEqual(status, 400, body)
        self.assertEqual(json.loads(body)['error']['param'], 'n')
        self.assertEqual(self.upstream.requests, [])
        self.assertEqual(self.chat(n=None, max_tokens=1000)[0], 200)
        self.assertEqual(len(self.upstream.requests), 1)

    def test_missing_pricing_is_unknown_and_refused_before_upstream(self):
        catalog = profiles()
        del catalog[REMOTE]['pricing']
        catalog['synthetic-anthropic'] = next(iter(anthropic_models().values()))
        del catalog['synthetic-anthropic']['pricing']
        catalog['synthetic-openai'] = {'kind': 'remote/openai', 'upstream_model': 'o3',
                                       'context_window': 100000, 'max_output': 4096}
        self.start(models=catalog)
        for model in catalog:
            status, body = self.chat(model=model)
            self.assertEqual(status, 400, body)
            self.assertEqual(json.loads(body)['error']['param'], 'pricing')
        self.assertEqual(self.upstream.requests, [])

    def test_explicitly_free_model_has_zero_cost_with_a_configured_budget(self):
        catalog = profiles()
        del catalog[REMOTE]['pricing']
        catalog[REMOTE]['free'] = True
        self.start(models=catalog, budget=0.0001)
        self.assertEqual(self.chat()[0], 200)
        self.assertEqual(self.ledger_rows()[0]['cost_usd'], 0)

    def test_default_secrets_location_is_independent_of_install_layout(self):
        home = self.tmp / 'synthetic-home'
        config = home / '.config/oxc/llm/runtime.json'
        repo = self.tmp / 'synthetic-repo'
        (repo / '.git').mkdir(parents=True)
        (repo / '.git/HEAD').write_text('ref: refs/heads/main\n')
        layouts = [repo / 'oxc/src/oxc/runtime/cloud.py',
                   home / '.local/share/ai-stack/releases/synthetic/runtime/cloud.py',
                   home / '.local/share/ai-stack/runtime/cloud.py']
        for module in layouts:
            with self.subTest(layout=module.relative_to(self.tmp)), patch.object(cloud, '__file__', str(module)), \
                    patch.object(cloud.ctl, 'CONFIG', config):
                self.assertEqual(cloud.secrets_file({}), config.parent / 'cloud-secrets.json')
                with self.assertRaises(cloud.CloudError) as raised:
                    cloud.secrets_file({'cloud': {'secrets_file': str(repo / 'private.json')}})
                self.assertIn('inside a repository', str(raised.exception))
                self.assertNotIn(str(self.tmp), str(raised.exception))

    def test_anthropic_missing_sdk_and_invalid_effort_have_actual_error_status(self):
        self.start(models=anthropic_models())
        alias = 'anthropic/claude-sonnet-5-5'
        with patch.dict(sys.modules, {'anthropic': None}):
            status, body = self.chat(model=alias)
        self.assertEqual(status, 503, body)
        self.assertEqual(json.loads(body)['error']['code'], 'cloud_sdk_unavailable')
        status, body = self.chat(model=alias, reasoning_effort=3)
        self.assertEqual(status, 400, body)
        self.assertEqual(self.upstream.requests, [])
