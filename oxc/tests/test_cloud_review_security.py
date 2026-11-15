"""Review fixes: LAN-share guests never reach cloud models; a cloud alias never shadows a local model."""
import json
from pathlib import Path
from types import SimpleNamespace

from oxc.runtime import cloud

from openai_api_stack import ORNITH
from test_cloud_remote import REMOTE, CloudCase, profiles


class LanShareGuest(CloudCase):
    def guest(self, **extra):
        self.start(cloud_extra=extra)
        def handle(handler, inner):  # the gate's effect on the controller: a hashed guest client
            handler.share_client = 'abc123'
            return inner()
        self.stack.server.share_listener = SimpleNamespace(
            controller=SimpleNamespace(opts={'max_tokens': 100}, handle=handle))

    def test_guest_chat_for_a_cloud_model_is_refused_before_upstream(self):
        self.guest()
        status, body = self.chat()
        self.assertEqual(status, 403)
        error = json.loads(body)['error']
        self.assertEqual((error['type'], error['code']), ('permission_error', 'cloud_not_shared'))
        self.assertEqual(self.upstream.requests, [])
        self.assertEqual(self.ledger_rows() if self.ledger.exists() else [], [])

    def test_guest_listings_omit_cloud_rows(self):
        self.guest()
        self.assertNotIn(REMOTE, [row['id'] for row in self.rows()])
        status, body = self.call('GET', '/_local/models')
        self.assertEqual(status, 200)
        self.assertNotIn(REMOTE, [row['id'] for row in json.loads(body)['data']])
        status, _ = self.call('GET', '/v1/models/' + REMOTE)
        self.assertEqual(status, 404)

    def test_owner_still_gets_cloud_rows(self):
        self.start()
        self.assertIn(REMOTE, [row['id'] for row in self.rows()])
        status, body = self.call('GET', '/_local/models')
        self.assertIn(REMOTE, [row['id'] for row in json.loads(body)['data']])


class LanShareOptIn(LanShareGuest):
    def owner_chat(self):
        # the owner's request on the same stack: no share_client, so not a guest
        guest = self.stack.server.share_listener
        self.stack.server.share_listener = None
        try:
            return self.chat()
        finally:
            self.stack.server.share_listener = guest

    def test_opt_in_allows_lists_and_marks_the_ledger_row(self):
        self.guest(share_with_lan=True)
        self.assertIn(REMOTE, [row['id'] for row in self.rows()])
        status, body = self.call('GET', '/_local/models')
        self.assertIn(REMOTE, [row['id'] for row in json.loads(body)['data']])
        status, _ = self.call('GET', '/v1/models/' + REMOTE)
        self.assertEqual(status, 200)
        status, body = self.chat()
        self.assertEqual(status, 200, body)
        row = self.ledger_rows()[-1]
        self.assertEqual((row['remote'], row['origin']), (True, 'remote'))
        status, body = self.call('GET', '/_local/status')
        self.assertTrue(json.loads(body)['cloud']['share_with_lan'])

    def test_status_shows_sharing_off_by_default(self):
        self.guest()
        status, body = self.call('GET', '/_local/status')
        self.assertFalse(json.loads(body)['cloud']['share_with_lan'])

    def test_lan_cap_refuses_guests_but_not_the_owner(self):
        # one request reserves up to 64000 * 1e-6 + 4096 * 1e-6 = 0.068 USD in the worst case
        self.guest(share_with_lan=True, lan_daily_budget_usd=0.07)
        self.assertEqual(self.chat(max_tokens=1)[0], 200)
        row = self.ledger_rows()[-1]
        self.assertGreater(row['cost_usd'], 0)
        cap = row['cost_usd']
        # lower the cap to what is spent: the next guest request must be refused
        value = json.loads(Path(cloud.ctl.CONFIG).read_text())
        value['cloud']['lan_daily_budget_usd'] = cap
        Path(cloud.ctl.CONFIG).write_text(json.dumps(value))
        status, body = self.chat()
        self.assertEqual(status, 429, body)
        self.assertEqual(json.loads(body)['error']['code'], 'budget_exceeded')
        status, body = self.owner_chat()
        self.assertEqual(status, 200, body)


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
