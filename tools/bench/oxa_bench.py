"""Framework-neutral core of the oxa benchmark adapters (Harbor 0.22.0 and Pier 0.3.1).

Pure functions only: nothing here imports Harbor or Pier, touches Docker or the network. The two
adapters (`harbor_oxa.py`, `pier_oxa.py`) call these to build the argv, the in-container
settings, the run script and the post-run collection, so one test covers both.

Key rule: the container never sees the real model key. Its secrets file holds a stub and its
OpenCode base URL points at a host-side proxy that adds the real key.
"""

import hashlib
import ipaddress
import json
import os
import re
import shlex
from urllib.parse import urlsplit

DEFAULT_MODEL = "opencode/kimi-k3"
DEFAULT_AGENT_TIMEOUT_SEC = 3600
DEFAULT_MARGIN_SEC = 120
STUB_KEY = "fh-stub"
UPSTREAM_HOST = "opencode.ai"

REMOTE_BINARY = "/usr/local/bin/oxa"
REMOTE_UPLOAD = "/tmp/oxa-bench-upload"
REMOTE_LOGS = "/logs/agent"
REMOTE_USAGE_LOG = f"{REMOTE_LOGS}/oxa-usage.jsonl"
REMOTE_STDOUT = f"{REMOTE_LOGS}/oxa.jsonl"
REMOTE_STDERR = f"{REMOTE_LOGS}/oxa.stderr"
REMOTE_EXIT = f"{REMOTE_LOGS}/oxa.exit"
COLLECTED = ("oxa-usage.jsonl", "oxa.jsonl", "oxa.stderr", "oxa.exit")

# oxa exit codes that end a run the verifier should still score (0 done, 1 failed after model
# calls, 130 interrupted by --max-time). Anything else (budget, credentials, ledger, offline,
# setup refusal, a crash) is an infrastructure failure and is passed on as a non-zero exit.
SCORED_EXIT_CODES = (0, 1, 130)


class BenchError(ValueError):
    """The adapter was configured in a way that cannot work."""


def first(*values):
    """The first value that is not None."""
    return next((v for v in values if v is not None), None)


def resolve_max_time(agent_timeout_sec, margin_sec=DEFAULT_MARGIN_SEC):
    """`--max-time` for oxa: the task's agent limit minus a margin for oxa's wrap-up turn."""
    timeout = float(agent_timeout_sec)
    margin = float(margin_sec)
    if margin < 0:
        raise BenchError("margin must not be negative")
    limit = timeout - margin
    if limit < 1:
        raise BenchError(f"agent timeout {timeout:g}s leaves no time after the {margin:g}s margin")
    return int(limit) if limit == int(limit) else limit


def check_proxy_url(url):
    """A proxy URL accepted by oxa's native OpenCode transport."""
    parts = urlsplit(url or "")
    if parts.scheme not in ("http", "https") or not parts.hostname:
        raise BenchError("proxy URL must be http(s)://host:port (set OXA_BENCH_PROXY_URL)")
    try:
        parts.port
    except ValueError as error:
        raise BenchError("proxy URL has an invalid port") from error
    if parts.hostname.lower().rstrip(".").endswith(UPSTREAM_HOST):
        raise BenchError("proxy URL points at the real provider: containers must use the key-injecting proxy")
    if parts.username or parts.password:
        raise BenchError("proxy URL must not carry credentials")
    if "?" in url or "#" in url:
        raise BenchError("proxy URL must not carry a query or fragment")
    try:
        address = ipaddress.ip_address(parts.hostname)
        mapped = getattr(address, "ipv4_mapped", None)
        loopback = address.is_loopback or bool(mapped and mapped.is_loopback)
    except ValueError:
        loopback = parts.hostname.lower() == "localhost"
    if parts.scheme == "http" and not loopback:
        raise BenchError("remote proxy URL must use https (http is accepted only on loopback)")
    return url.rstrip("/")


def proxy_host(url):
    """Bare host of the proxy URL, for a network allowlist (Pier takes no ports)."""
    return urlsplit(check_proxy_url(url)).hostname.lower()


def base_url(proxy_url, trial_id=None):
    """The OpenCode base URL oxa gets: the proxy, with an optional per-trial path prefix."""
    url = check_proxy_url(proxy_url)
    if trial_id:
        if not re.fullmatch(r"[A-Za-z0-9._-]+", trial_id):
            raise BenchError("trial id may only hold letters, digits, dot, dash and underscore")
        url = f"{url}/{trial_id}"
    return url


def settings_json(url):
    """`~/.oxa/agent/settings.json` inside the container. Defaults everywhere else."""
    return json.dumps(
        {
            "oxa": {
                "engine": {"inProcessCloud": True},
                "cloud": {
                    "opencode": {"enabled": True, "base_url": url},
                    "subscription_limits": {"daily_requests": 1000000, "daily_tokens": 100000000000},
                },
            }
        },
        indent=2,
    ) + "\n"


def secrets_json():
    """`~/.config/oxc/llm/cloud-secrets.json` inside the container: the stub, never a real key."""
    return json.dumps({"opencode": STUB_KEY}, indent=2) + "\n"


def configure_command(url):
    """Shell command (agent user) that writes both files fresh, the secrets file mode 0600."""
    return (
        "set -e; umask 077; mkdir -p \"$HOME/.oxa/agent\" \"$HOME/.config/oxc/llm\"; "
        f"printf %s {shlex.quote(settings_json(url))} > \"$HOME/.oxa/agent/settings.json\"; "
        f"printf %s {shlex.quote(secrets_json())} > \"$HOME/.config/oxc/llm/cloud-secrets.json\"; "
        "chmod 600 \"$HOME/.config/oxc/llm/cloud-secrets.json\""
    )


def install_command():
    """Shell command (root) that puts the uploaded binary on PATH and proves it starts."""
    return (
        f"install -m 0755 {REMOTE_UPLOAD} {REMOTE_BINARY} && rm -f {REMOTE_UPLOAD} && {REMOTE_BINARY} --version"
    )


def build_argv(instruction, model=DEFAULT_MODEL, max_time=None):
    """The oxa argv: headless, yolo, JSON events, default settings otherwise."""
    if not instruction:
        raise BenchError("empty instruction")
    if max_time is None:
        raise BenchError("max_time is required")
    return [
        "oxa", "-p", "--yolo", "--mode", "json",
        "--model", model,
        "--no-context-files", "--no-mcp", "--no-web-search", "--no-session",
        "--max-time", str(max_time),
        "--usage-log", REMOTE_USAGE_LOG,
        "--",
        instruction,
    ]


def run_script(argv):
    """Capture the run and pass only completed, scorable outcomes as zero."""
    return (
        f"mkdir -p {REMOTE_LOGS}; "
        f"{shlex.join(argv)} > {REMOTE_STDOUT} 2> {REMOTE_STDERR} < /dev/null; rc=$?; "
        f"printf '%s\\n' \"$rc\" > {REMOTE_EXIT}; "
        "case $rc in "
        "0|130) exit 0;; "
        "1) "
        f"test -s {REMOTE_USAGE_LOG} && "
        f"grep -Eq '\"request\"[[:space:]]*:[[:space:]]*[1-9][0-9]*' {REMOTE_USAGE_LOG} && "
        f"grep -q '\"oxa_result\"' {REMOTE_STDERR} && "
        f"grep -Eq '\"requests\"[[:space:]]*:[[:space:]]*[1-9][0-9]*' {REMOTE_STDERR} && "
        "exit 0; exit 1;; "
        "*) exit $rc;; esac"
    )


def run_env(allow_home_cwd=False):
    """Extra env for the run. Only what is needed; no key of any kind."""
    return {"OXA_ALLOW_HOME_CWD": "1"} if allow_home_cwd else {}


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_binary(path, expected_sha256=None):
    """Return the binary's sha256, checking `expected_sha256` or a sibling `oxa.sha256` if present."""
    if not os.path.isfile(path):
        raise BenchError(f"oxa binary not found: {path}")
    actual = sha256_file(path)
    expected = expected_sha256
    sibling = os.path.join(os.path.dirname(os.path.abspath(path)), "oxa.sha256")
    if expected is None and os.path.basename(path) == "oxa" and os.path.isfile(sibling):
        with open(sibling, encoding="utf-8") as handle:
            expected = handle.read().split()[0]
    if expected is not None and expected.lower() != actual:
        raise BenchError(f"oxa binary checksum mismatch: expected {expected}, got {actual}")
    return actual


def result_record(stderr_text):
    """The last `oxa_result` JSON line of oxa's stderr, or None."""
    for line in reversed(stderr_text.splitlines()):
        line = line.strip()
        if line.startswith("{") and '"oxa_result"' in line:
            try:
                return json.loads(line)
            except ValueError:
                return None
    return None


def usage_totals(logs_dir):
    """Token totals for the harness context from the collected usage log, or None."""
    import usage_details_oxa

    path = os.path.join(logs_dir, "oxa-usage.jsonl")
    if not os.path.isfile(path):
        return None
    try:
        details = usage_details_oxa.parse_usage_log(path)
    except usage_details_oxa.UsageLogError:
        return None
    return details


def missing_files(logs_dir):
    """Collected files not yet on the host (the framework did not sync them)."""
    return [name for name in COLLECTED if not os.path.exists(os.path.join(logs_dir, name))]
