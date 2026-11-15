"""CLI peer isolation must also work when a hosted checkout lives under /home."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

from environment import ROOT, isolated_env


class CliNamespaceContract(unittest.TestCase):
    def test_checkout_under_private_home_remains_available(self):
        checkout = Path('/home/oxc-checkout')  # Synthetic mount, never a real account.
        with tempfile.TemporaryDirectory(prefix='oxc-cli-checkout-') as directory:
            home = Path(directory)
            env = isolated_env(home)
            if 'OXC_COMMAND' in os.environ:
                words = shlex.split(os.environ['OXC_COMMAND'])
                words = [str(checkout) + word[len(str(ROOT)):] if word.startswith(str(ROOT) + '/') else word
                         for word in words]
                env['OXC_COMMAND'] = shlex.join(words)
            program = ('import json,sys; sys.path.insert(0,' + repr(str(checkout / 'conformance')) + '); '
                       'from cli_harness import invoke_case; '
                       'print(json.dumps(invoke_case("unload",script=' + repr(str(checkout / 'conformance/test_cli_control.py')) + ')))')
            result = subprocess.run([
                'bwrap', '--die-with-parent', '--unshare-net', '--ro-bind', '/', '/',
                '--dev-bind', '/dev', '/dev', '--bind', '/proc', '/proc',
                '--tmpfs', '/home', '--ro-bind', str(ROOT), str(checkout), '--tmpfs', '/tmp',
                '--', sys.executable, '-c', program,
            ], env=env, capture_output=True, text=True, timeout=10)
            if result.returncode:
                raise AssertionError('Private checkout contract failed: ' + result.stderr)
            value = json.loads(result.stdout)
            self.assertEqual(value, {'exit': 0, 'stdout': '', 'stderr': '',
                                    'requests': [{'method': 'POST', 'path': '/_local/unload', 'body': {}}],
                                    'host_commands': []})


if __name__ == '__main__':
    unittest.main()
