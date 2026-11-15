"""Synthetic profile builders for retained runtime policy tests; no staging entrypoint."""

import copy

IO_MODES = ('qos', 'spin')

IDLE_QOS = {'unit': 'oxc-flash-qos@{pid}.service', 'cpu': 8, 'latency_us': 100}

ALIAS = 'qwen3.8-flash-next-local'

THREADS = 8

ADMITTED_BUDGET_BYTES = 28 * 1024**3

SESSION_RESERVE_BYTES = 4 * 128 * 1024**2

FLASH_ARGS = ['--engine', 'flash', '--model', '{model}', '--alias', '{alias}', '--ctx-size', '{context}',
              # IO thread on core 8; compute threads avoid it (see ENVIRONMENT GOMP_CPU_AFFINITY).
              '--flash-threads', '8', '--flash-threads-batch', '11', '--flash-io-cpu', '8',
              '--flash-cache-gib', '20', '--ctx-checkpoints', '4',
              '--session-ram-bytes', '0', '--reasoning', 'off']

WARMUP_FLAGS = ('--flash-hotset', '--prefix-cache', '--prefix-cache-min-tokens', '--session-cache')

ENVIRONMENT = {'GOMP_CPU_AFFINITY': '0-7 9-11', 'OMP_WAIT_POLICY': 'ACTIVE', 'OMP_PROC_BIND': 'TRUE',
               'OXC_ENGINE_LEDGER': '1'}

SPIN_ENVIRONMENT = {'OXF_IO': 'spins=1000000'}

def check_no_warmup(args):
    if any(arg.split('=', 1)[0] in WARMUP_FLAGS for arg in args) or _session_ram(args) != '0':
        raise ValueError('flash_warmup_paths_must_stay_off')

def _session_ram(args):
    return args[args.index('--session-ram-bytes') + 1] if '--session-ram-bytes' in args else None

def flash_override(server_binary, server_sha256, *, sidecar_path, sidecar_bytes, measured_peak_bytes, cache_gib,
                   fallback_peak_bytes=None, budget_bytes=ADMITTED_BUDGET_BYTES, qualified=True, io_mode='qos',
                   minimum_available_bytes=None):
    if io_mode not in IO_MODES:
        raise ValueError('flash_io_mode_invalid')
    args = [str(cache_gib) if i and FLASH_ARGS[i - 1] == '--flash-cache-gib' else arg
            for i, arg in enumerate(FLASH_ARGS)]
    check_no_warmup(args)
    # the engine reads the staged sidecar (it must match sidecar_path: the controller stats that file at load)
    args += ['--flash-sidecar', str(sidecar_path)]
    extra = {} if fallback_peak_bytes is None else {'fallback': {'cache_gib': 18, 'admission': {
        'measured_peak_bytes': fallback_peak_bytes, 'admitted_budget_bytes': budget_bytes}}}
    # qos: the idle-QoS hook switches on (the root helper owns the knob); spin: the fallback, no hook.
    qos = {'idle_qos': copy.deepcopy(IDLE_QOS)} if io_mode == 'qos' else {}
    environment = {**ENVIRONMENT, **({} if io_mode == 'qos' else SPIN_ENVIRONMENT)}
    kind = 'oxe-rust-flash-qualification' if qualified else 'oxe-rust-flash-candidate'
    admission = {'measured_peak_bytes': measured_peak_bytes, 'admitted_budget_bytes': budget_bytes}
    if minimum_available_bytes is not None:
        admission['minimum_available_bytes'] = minimum_available_bytes
    return {**extra, **qos, 'server_binary': server_binary, 'server_sha256': server_sha256, 'threads': THREADS,
            'oxe_args': args, 'environment': environment,
            'session_reserve_bytes': SESSION_RESERVE_BYTES,
            'validation': {'kind': kind, 'server_sha256': server_sha256,
                           'report': 'docs/FLASH-RUST-PRODUCTION.md', 'qualified': qualified},
            'cache_gib': cache_gib, 'sidecar_path': str(sidecar_path), 'sidecar_bytes': sidecar_bytes,
            'admission': admission}
