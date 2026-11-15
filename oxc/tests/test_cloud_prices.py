"""Unauthenticated price metadata against a loopback server and isolated state roots."""
import json
import os
from pathlib import Path
import stat
import tempfile
import threading
import unittest
from unittest.mock import patch

from mock_cloud import _Base
from oxc.runtime import cloud, cloud_prices
from test_cloud_remote import profiles, REMOTE, UPSTREAM


class PriceServer(_Base):
    def __init__(self):
        super().__init__()
        self.rows = [{'id': UPSTREAM, 'pricing': {'prompt': '0.000003', 'completion': '0.000005',
                                               'input_cache_read': '0.0000003',
                                               'input_cache_write': '0.000004'}}]
        self.status = 200
        self.start()

    def handle(self, handler, method):
        self.requests.append({'method': method, 'path': handler.path, 'headers': dict(handler.headers)})
        self._json(handler, self.status, {'data': self.rows})


class PriceRefresh(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / 'state' / 'prices.json'
        self.server = PriceServer()
        self.addCleanup(self.server.close)
        self.url = self.server.base + '/api/v1/models'

    def refresh(self):
        return cloud_prices.refresh(self.path, url=self.url)

    def test_public_metadata_is_cached_without_auth_and_survives_restart(self):
        self.assertTrue(self.refresh())
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
        sent = self.server.requests[0]
        self.assertEqual((sent['method'], sent['path']), ('GET', '/api/v1/models'))
        self.assertNotIn('Authorization', sent['headers'])
        with patch.object(cloud_prices, '_snapshots', {}), patch.object(
                cloud_prices, '_fetch', side_effect=AssertionError('offline must not fetch')):
            rates = cloud_prices.price_for(UPSTREAM, self.path)
        self.assertEqual(rates['input_per_million'], 3)
        self.assertEqual(rates['cache_write_per_million'], 4)

    def test_failed_malformed_missing_or_empty_refresh_keeps_known_prices(self):
        self.assertTrue(self.refresh())
        before = self.path.read_bytes()
        for status, rows in ((503, []), (200, []), (200, [{'id': UPSTREAM, 'pricing': {'prompt': 'NaN'}}]),
                             (200, [{'id': UPSTREAM, 'pricing': {'prompt': '-1', 'completion': '0'}}])):
            with self.subTest(status=status, rows=rows):
                self.server.status, self.server.rows = status, rows
                self.assertFalse(self.refresh())
                self.assertEqual(self.path.read_bytes(), before)
                self.assertEqual(cloud_prices.price_for(UPSTREAM, self.path)['input_per_million'], 3)

    def test_oversized_or_redirected_reply_does_not_replace_known_prices(self):
        self.assertTrue(self.refresh())
        before = self.path.read_bytes()
        self.server.status = 302
        self.assertFalse(self.refresh())
        self.assertEqual(self.path.read_bytes(), before)
        self.server.status = 200
        self.server.rows[0]['pricing']['unused'] = 'x' * (cloud_prices.MAX_BYTES + 1)
        self.assertFalse(self.refresh())
        self.assertEqual(self.path.read_bytes(), before)

    def test_fetched_prices_replace_configured_rates(self):
        self.assertTrue(self.refresh())
        configured = {'tiers': [{'max_prompt_tokens': 100, 'input_per_million': 1,
                                 'output_per_million': 10},
                                {'max_prompt_tokens': None, 'input_per_million': 7,
                                 'output_per_million': 20}]}
        with patch.object(cloud_prices, 'cache_path', return_value=self.path):
            value = cloud_prices.overlay(configured, UPSTREAM)
            self.assertEqual(value['tiers'][0]['input_per_million'], 3)
            self.assertEqual(value['tiers'][0]['output_per_million'], 5)
            self.assertEqual(value['tiers'][1]['input_per_million'], 3)
            self.assertEqual(value['tiers'][1]['max_prompt_tokens'], None)
            self.assertEqual(cloud_prices.overlay({'input_per_million': 99, 'output_per_million': 99},
                                                  UPSTREAM)['input_per_million'], 3)
            self.assertIsNone(cloud_prices.overlay(None, 'unknown/model'))
            raw = profiles()[REMOTE]
            raw.pop('pricing')
            self.assertEqual(cloud.entry(REMOTE, raw)['pricing']['input_per_million'], 3)

    def test_live_metadata_cannot_hide_an_invalid_configured_price_or_tier(self):
        self.assertTrue(self.refresh())
        invalid = ({'tiers': [{'max_prompt_tokens': 100, 'input_per_million': 100,
                              'output_per_million': 200}]},
                   {'input_per_million': -1, 'output_per_million': 2})
        with patch.object(cloud_prices, 'cache_path', return_value=self.path):
            for price in invalid:
                with self.subTest(price=price):
                    raw = profiles()[REMOTE]
                    raw['pricing'] = price
                    item = cloud.entry(REMOTE, raw)
                    self.assertIsNone(item['pricing'])
                    with self.assertRaises(cloud.CloudError):
                        cloud.validate_remote_request({}, item)

    def test_corrupt_or_symlink_cache_is_not_read_as_a_price(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_text('not json')
        self.assertIsNone(cloud_prices.price_for(UPSTREAM, self.path))
        self.path.unlink()
        other = self.path.parent / 'other.json'
        other.write_text(json.dumps({'data': self.server.rows}))
        self.path.symlink_to(other)
        with patch.object(cloud_prices, '_snapshots', {}):
            self.assertIsNone(cloud_prices.price_for(UPSTREAM, self.path))

    def test_group_writable_cache_is_not_trusted(self):
        self.assertTrue(self.refresh())
        self.path.chmod(0o660)
        with patch.object(cloud_prices, '_snapshots', {}):
            self.assertIsNone(cloud_prices.price_for(UPSTREAM, self.path))

    def test_disabled_refresher_never_fetches_and_daily_worker_stops(self):
        with patch.object(cloud_prices, 'refresh') as fetch:
            worker = cloud_prices.Refresher(lambda: False)
            worker.close()
            fetch.assert_not_called()
        twice = threading.Event()
        calls = []
        def refresh():
            calls.append(True)
            if len(calls) >= 2:
                twice.set()
            return True
        with patch.object(cloud_prices, 'DAY_SECONDS', 0.01), patch.object(
                cloud_prices, 'refresh', side_effect=refresh):
            worker = cloud_prices.Refresher(lambda: True)
            try:
                self.assertTrue(twice.wait(1))
            finally:
                worker.close()
            self.assertFalse(worker.thread.is_alive())

    def test_offline_and_cloud_off_block_the_startup_gate(self):
        from oxc.runtime import controller
        config = self.path.parent / 'runtime.json'
        config.parent.mkdir(parents=True)
        config.write_text(json.dumps({'cloud': {'enabled': True, 'models': profiles()}}))
        with patch.object(controller, 'CONFIG', config), patch.dict(os.environ, {'OXC_OFFLINE': '1'}):
            self.assertFalse(controller.cloud_price_refresh_allowed())
        with patch.object(controller, 'CONFIG', config), patch.dict(os.environ, {'OXC_OFFLINE': '0'}):
            self.assertTrue(controller.cloud_price_refresh_allowed())
            config.write_text(json.dumps({'cloud': {'enabled': False, 'models': profiles()}}))
            self.assertFalse(controller.cloud_price_refresh_allowed())
