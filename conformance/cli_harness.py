"""Model-free CLI peers in private network and HOME namespaces."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from environment import ROOT, isolated_env, PYTHON, python_dependency_mounts

CANONICAL_HOME = Path('/home/oxc-conformance')  # Synthetic namespace mount.

def invoke_case(name, script=None, network_admin=False, timeout=25):
    # A new network namespace per case gives the CLI its hardcoded 8092 without
    # ever reaching a service outside this synthetic installation.
    with tempfile.TemporaryDirectory(prefix='oxc-cli-runtime-') as directory:
        home = Path(directory)
        env = isolated_env(home)
        for key in ('OXC_COMMAND', 'OXC_DAEMON_COMMAND', 'OXC_CLOUD_INSTALL_COMMAND'):
            if key in os.environ:
                env[key] = os.environ[key]
        command = ['bwrap', '--die-with-parent', '--unshare-net',
                   *(['--unshare-user', '--cap-add', 'CAP_NET_ADMIN', '--cap-add', 'CAP_SETPCAP'] if network_admin else []), '--ro-bind', '/', '/',
                   '--dev-bind', '/dev', '/dev', '--bind', '/proc', '/proc',
                   '--tmpfs', '/home', '--tmpfs', '/tmp',
                   # Hosted checkouts can be inside /home (or /tmp). Keep just
                   # the source checkout visible after replacing those roots.
                   '--ro-bind', str(ROOT), str(ROOT),
                   *python_dependency_mounts(env),
                   '--bind', str(home), str(CANONICAL_HOME),
                   '--setenv', 'HOME', str(CANONICAL_HOME), '--',
                   PYTHON, str(Path(script or ROOT / 'conformance/test_cli_runtime.py').resolve()), '--case', name]
        result = subprocess.run(command, env=env, capture_output=True, timeout=timeout)
        if result.returncode:
            raise AssertionError(name + ': ' + result.stderr.decode())
        return json.loads(result.stdout)
