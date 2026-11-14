"""Model-free controller RSS/CPU/HTTP latency measurement; never a hardware A/B."""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import threading
import time

from harness import Daemon, PRIMARY, ROOT


def cpu_seconds(pid):
    # After the closing ')' fields begin with stat field 3 (state).
    fields = Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')


def rss_kib(pid):
    for line in Path('/proc', str(pid), 'status').read_text().splitlines():
        if line.startswith('VmRSS:'):
            return int(line.split()[1])
    raise RuntimeError('Controller RSS is unavailable')


def measure(requests, idle_seconds):
    with Daemon(configured=True) as daemon:
        pid = json.loads(daemon.child_info.read_text())['child-pid']
        payload = {'model': PRIMARY, 'messages': [{'role': 'user', 'content': 'Synthetic benchmark.'}],
                   'max_tokens': 3, 'stream': False}
        if daemon.call('POST', '/v1/chat/completions', payload)['status'] != 200:
            raise RuntimeError('Benchmark warmup failed')
        idle_rss = rss_kib(pid)
        before, start = cpu_seconds(pid), time.perf_counter()
        time.sleep(idle_seconds)
        idle_wall = time.perf_counter() - start
        idle_cpu = cpu_seconds(pid) - before
        rss = []
        stop = threading.Event()

        def sample():
            while not stop.is_set():
                rss.append(rss_kib(pid))
                stop.wait(0.02)
        sampler = threading.Thread(target=sample, daemon=True)
        sampler.start()
        latencies = []
        before, start = cpu_seconds(pid), time.perf_counter()
        try:
            for _ in range(requests):
                t0 = time.perf_counter()
                reply = daemon.call('POST', '/v1/chat/completions', payload,
                                    headers={'X-Local-AI-Origin': 'lab'})
                latencies.append((time.perf_counter() - t0) * 1000)
                if reply['status'] != 200:
                    raise RuntimeError('Benchmark request failed')
            load_wall = time.perf_counter() - start
            load_cpu = cpu_seconds(pid) - before
        finally:
            stop.set()
            sampler.join(timeout=2)
        sha = subprocess.run(['git', 'rev-parse', 'origin/main'], cwd=ROOT,
                             capture_output=True, text=True, check=True).stdout.strip()
        ordered = sorted(latencies)
        return {
            'schema': 'oxc-fake-engine-benchmark/1',
            'created_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'main_sha': sha,
            'implementation': 'selected' if os.environ.get('OXC_DAEMON_COMMAND') else 'python',
            'architecture': platform.machine(), 'available_cpus': len(os.sched_getaffinity(0)),
            'cpu_tick_seconds': 1 / os.sysconf('SC_CLK_TCK'),
            'fixture_sha256': hashlib.sha256((ROOT / 'conformance/fixtures/fake_engine.py').read_bytes()).hexdigest(),
            'requests': requests, 'stream': False, 'output_tokens_per_request': 3,
            'idle': {'wall_seconds': round(idle_wall, 4), 'rss_kib': idle_rss,
                     'cpu_seconds': round(idle_cpu, 4), 'cpu_percent_one_core': round(100 * idle_cpu / idle_wall, 3)},
            'load': {'wall_seconds': round(load_wall, 4), 'peak_sampled_rss_kib': max(rss),
                     'cpu_seconds': round(load_cpu, 4), 'cpu_percent_one_core': round(100 * load_cpu / load_wall, 3),
                     'latency_mean_ms': round(statistics.mean(latencies), 3),
                     'latency_p50_ms': round(statistics.median(latencies), 3),
                     'latency_p95_ms': round(ordered[math.ceil(len(ordered) * 0.95) - 1], 3)},
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--requests', type=int, default=100)
    parser.add_argument('--idle-seconds', type=float, default=1)
    args = parser.parse_args()
    if args.requests < 5 or not 0.1 <= args.idle_seconds <= 30:
        parser.error('use at least 5 requests and an idle interval from 0.1 to 30 seconds')
    print(json.dumps(measure(args.requests, args.idle_seconds), indent=2))


if __name__ == '__main__':
    main()
