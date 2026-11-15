"""Unit tests for the cloud secrets reader: every refusal says *why* without the key or content."""
import json
import os
import stat
import tempfile
import unittest
from unittest import mock
from pathlib import Path

from snapdragon_ai_lab.runtime import cloud

KEY = 'synthetic-cloud-key-for-mock-only'


def _value(path):
    return {'cloud': {'secrets_file': str(path)}}


class ReadSecrets(unittest.TestCase):
    """Direct read of the secrets file; the configuration path is always outside this repo."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='cloud-secrets-unit-'))
        self.secrets = self.tmp / 'cloud-secrets.json'
        self.secrets.write_text(json.dumps({'openrouter': {'api_key': KEY},
                                            'anthropic': {'api_key': KEY}}))
        os.chmod(self.secrets, stat.S_IRUSR | stat.S_IWUSR)

    def _refusal(self, path=None):
        with self.assertRaises(cloud.CloudError) as ctx:
            cloud._read_secrets(_value(path or self.secrets))
        return ctx.exception

    def test_a_good_file_reads_and_never_leaks(self):
        data = cloud._read_secrets(_value(self.secrets))
        self.assertEqual(data['openrouter']['api_key'], KEY)

    def test_a_symlinked_file_is_refused_with_a_clear_reason(self):
        link = self.tmp / 'link.json'
        link.symlink_to(self.secrets)
        exc = self._refusal(link)
        self.assertEqual(exc.code, 'cloud_credentials_unavailable')
        self.assertIn('symbolic link', str(exc))
        self.assertNotIn(KEY, str(exc))

    def test_a_directory_is_refused_as_not_a_regular_file(self):
        exc = self._refusal(self.tmp)
        self.assertEqual(exc.code, 'cloud_credentials_unavailable')
        self.assertIn('not a regular file', str(exc))

    def test_a_wrong_owner_is_refused_with_a_clear_reason(self):
        # Simulate another owner without needing root: patch getuid to disagree with st_uid.
        with mock.patch.object(cloud.os, 'getuid', return_value=os.getuid() + 1):
            exc = self._refusal()
        self.assertEqual(exc.code, 'cloud_credentials_unavailable')
        self.assertIn('owned by another user', str(exc))
        self.assertNotIn(KEY, str(exc))

    def test_a_wrong_mode_is_refused_with_a_clear_reason(self):
        os.chmod(self.secrets, 0o644)
        exc = self._refusal()
        self.assertEqual(exc.code, 'cloud_credentials_unavailable')
        self.assertIn('0600', str(exc))
        self.assertNotIn(KEY, str(exc))
        self.assertNotIn(str(self.secrets), str(exc))

    def test_a_too_large_file_is_refused_with_a_clear_reason(self):
        with self.secrets.open('wb') as stream:
            stream.write(b'{' + b' ' * cloud.MAX_SECRET_BYTES + b'}')
        os.chmod(self.secrets, stat.S_IRUSR | stat.S_IWUSR)
        exc = self._refusal()
        self.assertEqual(exc.code, 'cloud_credentials_unavailable')
        self.assertIn('larger than', str(exc))
        self.assertNotIn(KEY, str(exc))


if __name__ == '__main__':
    unittest.main()
