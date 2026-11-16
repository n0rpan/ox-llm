"""LAN-share cloud opt-in through the real listener; cloud aliases never shadow local models."""
import json
from unittest.mock import patch

from oxc.runtime import cloud, controller, lan_share

from openai_api_stack import ORNITH
from test_cloud_remote import REMOTE, CloudCase, profiles


class LanShareGuest(CloudCase):
    def guest(self):
        self.start()
        share = lan_share.ShareController(self.stack.config_path, 0, controller.Handler,
                                          manager=self.stack.manager, discover_fn=lambda: ([], []))
        self.stack.callback(share.close)
        # Ignore any owner's OXC_SHARE_KEY_FILE; only a throwaway share key is used here.
        self.stack.enter_context(patch.object(share, 'key_file', return_value=self.stack.tmp / 'share-key.json'))
        self.share_key = lan_share.ensure_key(share.key_file())['key']
        share.opts['max_tokens'] = 100
        listener = lan_share.Listener(share, '127.0.0.1', 0, controller.Handler, self.stack.manager)
        share.listeners[listener.address] = listener
        share.mode = 'tailscale'
        # CloudCase.call now talks to the real shared listener; Stack still owns and closes
        # its separate loopback server, and closes the shared listener first.
        self.stack.port = listener.port

    def call(self, method, path, body=None, headers=None):
        headers = dict(headers or {})
        if hasattr(self, 'share_key'):
            headers.setdefault('Authorization', 'Bearer ' + self.share_key)
        return super().call(method, path, body, headers)

    def share(self, value=True):
        config = json.loads(self.stack.config_path.read_text())
        config['cloud']['share_with_lan'] = value
        self.stack.config_path.write_text(json.dumps(config))

    def test_guest_chat_for_a_cloud_model_is_refused_before_upstream(self):
        self.guest()
        status, body = self.chat()
        self.assertEqual(status, 403)
        error = json.loads(body)['error']
        self.assertEqual((error['type'], error['code']), ('permission_error', 'cloud_not_shared'))
        self.assertIn('share_with_lan', error['message'])  # says how the owner can allow it
        self.assertEqual(self.upstream.requests, [])
        self.assertEqual(self.ledger_rows() if self.ledger.exists() else [], [])

    def test_guest_listings_omit_cloud_rows(self):
        self.guest()
        self.assertNotIn(REMOTE, [row['id'] for row in self.rows()])
        status, _ = self.call('GET', '/_local/models')
        self.assertEqual(status, 403)  # admin routes are never shared, even with a valid key
        status, _ = self.call('GET', '/v1/models/' + REMOTE)
        self.assertEqual(status, 404)

    def test_a_value_other_than_true_does_not_share(self):
        self.guest()
        for value in ('true', 1, 'yes'):
            with self.subTest(value=value):
                self.share(value)
                self.assertEqual(self.chat()[0], 403)
        self.assertEqual(self.upstream.requests, [])

    def test_opt_in_lets_a_guest_use_and_list_api_key_cloud_models(self):
        self.guest()
        self.share()
        self.assertIn(REMOTE, [row['id'] for row in self.rows()])
        status, _ = self.call('GET', '/_local/models')
        self.assertEqual(status, 403)
        status, body = self.call('GET', '/v1/models/' + REMOTE)
        self.assertEqual(status, 200, body)
        self.assertEqual(json.loads(body)['id'], REMOTE)
        status, body = self.call('HEAD', '/v1/models/' + REMOTE)
        self.assertEqual((status, body), (200, b''))
        status, body = self.chat(max_tokens=10000)
        self.assertEqual(status, 200, body)
        self.assertEqual(len(self.upstream.requests), 1)
        self.assertEqual(self.upstream.requests[0]['body']['max_tokens'], 100)
        row = self.ledger_rows()[-1]  # the guest's request is a ledger row, marked as a guest's
        self.assertEqual((row['origin'], row['remote'], row.get('lan_guest')), ('remote', True, True))
        self.assertGreater(row['cost_usd'], 0)

    def test_opt_in_guest_spend_counts_against_the_daily_budget(self):
        self.guest()
        self.share()
        self.assertEqual(self.chat()[0], 200)
        config = json.loads(self.stack.config_path.read_text())
        config['cloud']['daily_budget_usd'] = 1e-9
        self.stack.config_path.write_text(json.dumps(config))
        status, body = self.chat()
        self.assertEqual(status, 429, body)
        self.assertEqual(len(self.upstream.requests), 1)

    def test_status_reports_whether_cloud_is_shared(self):
        self.start()  # owner-only status is read on the loopback listener
        self.assertIs(json.loads(self.call('GET', '/_local/status')[1])['cloud']['share_with_lan'], False)
        self.share()
        self.assertIs(json.loads(self.call('GET', '/_local/status')[1])['cloud']['share_with_lan'], True)

    def test_owner_still_gets_cloud_rows(self):
        self.start()
        self.assertIn(REMOTE, [row['id'] for row in self.rows()])
        status, body = self.call('GET', '/_local/models')
        self.assertIn(REMOTE, [row['id'] for row in json.loads(body)['data']])


class AliasCollision(CloudCase):
    def table(self, alias):
        row = dict(profiles()[REMOTE])
        return {alias: row}

    def test_cloud_alias_equal_to_a_local_alias_is_skipped_and_reported(self):
        self.start(models=self.table(ORNITH))
        self.assertNotIn(ORNITH, [row['id'] for row in self.rows() if row.get('remote')])
        status, body = self.chat(model=ORNITH)
        self.assertEqual(status, 200)  # served by the local engine (the same mock port), not the cloud route
        self.assertNotIn('authorization', self.upstream.requests[-1]['headers'])
        self.assertEqual(self.upstream.requests[-1]['body']['model'], ORNITH)
        status, body = self.call('GET', '/_local/status')
        self.assertIn(ORNITH, json.loads(body)['cloud']['rejected'])

    def test_prefix_forms_collide_too(self):
        for alias in ('openai/' + ORNITH, 'local/' + ORNITH):
            with self.subTest(alias=alias):
                value = {'model_alias': ORNITH, 'profiles': {ORNITH: {}},
                         'cloud': {'enabled': True, 'models': self.table(alias)}}
                self.assertEqual(cloud.entries(value), {})
        value = {'model_alias': ORNITH, 'cloud': {'enabled': True, 'models': self.table('mine')}}
        self.assertIn('mine', cloud.entries(value))

    def test_local_prefix_is_never_stripped_for_cloud_lookup(self):
        value = {'model_alias': 'x', 'cloud': {'enabled': True, 'models': self.table('mine')}}
        self.assertIsNotNone(cloud.model(value, 'openai/mine'))
        self.assertIsNone(cloud.model(value, 'local/mine'))
