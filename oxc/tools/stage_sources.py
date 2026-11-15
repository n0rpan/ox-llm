"""Shared source inventory and syntax checks; bodies preserved verbatim."""

from pathlib import Path

import py_compile

import tempfile

import stage_unit as base

REQUIRED = ('controller.py', 'ctl_seams.py', 'ctl_env.py', 'ctl_host.py', 'ctl_repack.py', 'ctl_status.py',
            'ctl_models.py', 'ctl_cache.py', 'ctl_lease.py', 'ctl_engines.py', 'ctl_lifecycle.py',
            'ctl_manager.py', 'ctl_routes.py', 'ctl_proxy.py', 'ctl_proxy_prep.py', 'ctl_proxy_stream.py',
            'ctl_proxy_ledger.py', 'ctl_queue.py', 'ctl_replay.py', 'ctl_diag.py', 'ctl_http.py', 'keep_warm.py',
            'openai_api.py', 'openai_conformance.py', 'openai_responses.py', 'harness_compat.py', 'harness_registry.py', 'request_ledger.py',
            'model_catalog.py', 'storage_view.py', 'engine_ledger.py', 'oxe_runtime.py',
            'profile_policy.py', 'memory_policy.py', 'exec_in_scope.py', 'cache_turn.py', 'cache_miss.py', 'state_cache.py',
            'lan_share.py')

CLI_NAME = 'oxc'

LEGACY_CLI = 'local-ai'

def cli_source(sources):
    """The CLI file of a release: the current name, or the pre-rename name in an archived revision."""
    return sources[CLI_NAME] if CLI_NAME in sources else sources[LEGACY_CLI]

def release_sources(source_dir, required=REQUIRED):
    sources = {p.name: p.read_bytes() for p in sorted(Path(source_dir).iterdir())
               if p.is_file() and (p.suffix == '.py' or p.name in base.RUNTIME_FILES or p.name == LEGACY_CLI)}
    missing = [name for name in required if name not in sources]
    if missing:
        raise ValueError('release_is_missing: ' + ', '.join(missing))
    with tempfile.TemporaryDirectory() as tmp:  # a syntax error must not reach a stage
        for name, data in sources.items():
            if name.endswith('.py'):
                probe = Path(tmp) / name
                probe.write_bytes(data)
                py_compile.compile(str(probe), cfile=str(probe) + 'c', doraise=True)
    return sources
