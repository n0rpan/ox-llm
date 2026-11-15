"""Synthetic previous-release layouts for retained stage tests; never invokes a legacy generator."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import stage_shared as gen
ENGINE_REV = '6' * 40
OLD_SERVER = b'v012 oxe-server'
MOE = {'ornith-35b-a3b-local': ('a' * 64, '1073741824', '32768'), 'qwen3.6-35b-a3b-local': ('b' * 64, '536870912', '16384')}


def h(data):
    return hashlib.sha256(data).hexdigest()

def have_git_head(rev='HEAD'):
    try:
        subprocess.run(['git', '-C', str(ROOT), 'rev-parse', '--verify', rev + '^{commit}'], check=True, capture_output=True)
        return True
    except (OSError, subprocess.CalledProcessError):
        return False

def tree(path):
    return {p.relative_to(path).as_posix(): (h(p.read_bytes()), p.stat().st_mode & 0o7777)
            for p in sorted(Path(path).rglob('*')) if p.is_file()}

class StageLayout:
    def runtime(self, extra_args=(), extra_env=None):
        engine_dir = str(self.home / gen.ENGINES / 'oxe-v012-final-old')
        profiles = {}
        for alias, (model_sha, ram, mtp) in MOE.items():
            env = {'OXE_ATTN_TILED_MIN_KV': '0', 'OXE_VL': '15', 'OXC_ENGINE_LEDGER': '1', **(extra_env or {})}
            profiles[alias] = {
                'display_name': 'Ornith 1.5 35B A3B' if alias.startswith('ornith') else 'Qwen 3.6 35B MoE',
                'sha256': model_sha, 'context_size': 16384, 'threads': 4,
                'memlat_overrides': {'oxe': {
                    'server_binary': engine_dir + '/oxe-server', 'server_sha256': h(OLD_SERVER),
                    'context_size': 131072, 'threads': 8,
                    'oxe_args': ['--engine', 'oxe', '--ctx-size', '{context}', '--oxe-cpus', '0-7',
                                 '--oxe-prefill-cpus', '0-11', '--ctx-checkpoints', '8', '--session-ram-bytes', ram,
                                 '--park-disk-min-bytes', '536870912', '--mtp-max-ctx', mtp, *extra_args],
                    'environment': env,
                    'validation': {'kind': 'v012-final-staged-unqualified', 'qualified': False}}}}
        profiles[gen.FLASH] = {'display_name': 'Flash', 'memlat_overrides': {'oxe_flash': {
            'cache_pref': '18', 'prefix_store': {'enabled': True}, 'sidecar_path': '/x/experts-r4.bin',
            'server_sha256': 'f' * 64,
            'oxe_args': ['--flash-sidecar', '/x/experts-r4.bin', '--flash-psi-some', '0', '--flash-psi-full', '0']}}}
        return {'model_alias': 'ornith-35b-a3b-local', 'selected_model': 'ornith-35b-a3b-local',
                'profiles': profiles, 'cache': dict(gen.CACHE)}

    def chat_unit(self):
        release = self.home / gen.RELEASES / 'v012-final-9e1fceca421a'
        return '\n'.join([
            '[Unit]', 'Description=Private local AI runtime (controller.py)', '[Service]', gen.WATCHDOG,
            'Environment=OXC_IMAGE_NPU=1', 'Type=simple', 'WorkingDirectory=%s' % release,
            'ExecStart=/usr/bin/python3 "%s/controller.py"' % release,
            'Environment="OXC_LEASE=/x/inference.lock"', 'Environment="LOCAL_LLM_CONFIG=/x/runtime.json"',
            'Environment=PYTHONDONTWRITEBYTECODE=1', 'Restart=on-failure', 'MemoryMax=25G', '[Install]',
            'WantedBy=default.target']) + '\n'

    def studio_unit(self):
        release = self.home / gen.RELEASES / 'studio-handoff-a2930ab1976cdf969e7d'
        return '\n'.join([
            '[Unit]', 'Description=Private local AI runtime (studio)', '[Service]', 'Type=simple',
            'WorkingDirectory=%s' % release, 'ExecStart=/usr/bin/python3 "%s/studio"' % release,
            'Environment="OXC_LEASE=/x/inference.lock"', 'Environment="LOCAL_IMAGE_CONFIG_DIR=/x/images"',
            'Restart=on-failure', 'RestartSec=5', 'MemoryMax=22G', '[Install]', 'WantedBy=default.target']) + '\n'

    def studio_dropin(self):
        release = self.home / gen.RELEASES / 'studio-npu-w1-7de44783af15'
        return '\n'.join(['[Service]', 'WorkingDirectory=%s' % release, 'ExecStart=',
                          'ExecStart=/usr/bin/python3 "%s/studio"' % release,
                          'Environment="OXI_BIN=/x/releases/oxi-fc267f50a3e0/oxi"',
                          'Environment="SD_CLI_PROBE=/x/releases/sd-probe-4a47a2659438/sd-cli"',
                          'CPUQuota=1200%']) + '\n'

    def make_engine(self, d, server=b'v013 oxe-server', oxe=b'v013 oxe cli'):
        (d / 'default').mkdir(parents=True)
        (d / 'default' / 'oxe-server').write_bytes(server)
        (d / 'default' / 'oxe').write_bytes(oxe)
        (d / 'manifest.json').write_text(json.dumps({'passed': True, 'source': ENGINE_REV, 'jobs': [
            {'kind': 'default', 'returncode': 0, 'command': ['cargo', 'build'], 'artifacts': {
                'oxe-server': {'sha256': h(server)}, 'oxe': {'sha256': h(oxe)}}}]}))
        return d
