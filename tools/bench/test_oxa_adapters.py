"""Fake-engine smoke test for the Harbor and Pier oxa adapters.

No network, no Docker, no Harbor or Pier install: the two frameworks' base classes are replaced
by a few lines of stubs, the "container" is a temp directory, and `oxa` is a shell script that
records its argv and writes the fixture usage log. Run: python3 -I -m unittest discover -s tools/bench
"""

import asyncio
import importlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import oxa_bench as core  # noqa: E402
import usage_details_oxa  # noqa: E402

FAKE_OXA = """#!/bin/sh
# fake oxa: record argv, write the fixture usage log, print an oxa_result line, exit as told
[ "$1" = "--version" ] && { echo "oxa 0.0.0-fake"; exit 0; }
printf '%s\\n' "$@" > "$FAKE_ARGV_FILE"
env | grep -c 'OXA_ALLOW_HOME_CWD=1' > "$FAKE_ARGV_FILE.env" || true
[ "${FAKE_OXA_STARTUP_FAILURE:-0}" = 1 ] && exit "${FAKE_OXA_EXIT:-1}"
while [ $# -gt 0 ]; do
  [ "$1" = "--usage-log" ] && cp "$FAKE_USAGE_FIXTURE" "$2"
  shift
done
echo '{"type":"agent_end"}'
echo '{"type":"oxa_result","turns":3,"requests":3}' >&2
exit "${FAKE_OXA_EXIT:-0}"
"""


class FakeResult:
    def __init__(self, proc):
        self.return_code, self.stdout, self.stderr = proc.returncode, proc.stdout, proc.stderr


class FakeEnvironment:
    """A temp directory standing in for the container; exec runs a local shell."""

    default_user = None

    def __init__(self, root, env):
        self.root, self.env, self.uploads = root, env, []

    def agent_process_env(self, env):
        return env

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        full = dict(os.environ, **self.env, **(env or {}))
        proc = subprocess.run(["bash", "-c", command], capture_output=True, text=True, env=full,
                              cwd=cwd or self.root)
        return FakeResult(proc)

    async def upload_file(self, source, target):
        self.uploads.append(str(target))
        shutil.copy(source, target)

    async def download_file(self, source, target):
        shutil.copy(source, target)


def _stub(name, **attrs):
    module = types.ModuleType(name)
    module.__dict__.update(attrs)
    sys.modules[name] = module
    return module


def _install_stubs():
    """Minimal stand-ins for the harbor and pier modules the adapters import."""
    import logging

    class Context:
        n_input_tokens = n_cache_tokens = n_output_tokens = None

    class HarborBase:
        def __init__(self, logs_dir, prompt_template_path=None, version=None, extra_env=None, *args,
                     model_name=None, **kwargs):
            self.logs_dir, self.model_name = logs_dir, model_name
            self._extra_env = dict(extra_env or {})
            self.logger = logging.getLogger("fake")

        def _get_env(self, key):
            return self._extra_env.get(key, os.environ.get(key))

        async def _exec(self, environment, command, user=None, env=None):
            result = await environment.exec(command=f"set -o pipefail; {command}", user=user, env=env)
            if result.return_code != 0:
                raise RuntimeError(f"exit {result.return_code}: {result.stderr}")
            return result

        async def exec_as_root(self, environment, command, env=None):
            return await self._exec(environment, command, user="root", env=env)

        async def exec_as_agent(self, environment, command, env=None):
            return await self._exec(environment, command, env=env)

    class PierBase:
        def __init__(self, logs_dir, model_name=None, *args, **kwargs):
            self.logs_dir, self.model_name = logs_dir, model_name
            self.logger = logging.getLogger("fake")

    class Allowlist:
        def __init__(self, domains):
            self.domains = domains

    for name in ("harbor", "harbor.agents", "harbor.agents.installed", "harbor.environments",
                 "harbor.models", "harbor.models.agent", "pier", "pier.agents", "pier.environments",
                 "pier.models", "pier.models.agent"):
        _stub(name)
    _stub("harbor.agents.installed.base", BaseInstalledAgent=HarborBase, with_prompt_template=lambda fn: fn)
    _stub("harbor.environments.base", BaseEnvironment=object)
    _stub("harbor.models.agent.context", AgentContext=Context)
    _stub("pier.agents.base", BaseAgent=PierBase)
    _stub("pier.environments.base", BaseEnvironment=object)
    _stub("pier.models.agent.context", AgentContext=Context)
    _stub("pier.models.agent.network", NetworkAllowlist=Allowlist)
    return Context


class AdapterTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        real = {k: sys.modules.get(k) for k in list(sys.modules) if k.split(".")[0] in ("harbor", "pier")}
        for key in real:
            del sys.modules[key]
        cls.Context = _install_stubs()
        cls.harbor = importlib.import_module("harbor_oxa")
        cls.pier = importlib.import_module("pier_oxa")
        for key in [k for k in sys.modules if k.split(".")[0] in ("harbor", "pier")]:
            del sys.modules[key]
        sys.modules.update({k: v for k, v in real.items() if v is not None})

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        root = self.tmp / "container"
        for sub in ("bin", "logs/agent", "home", "tmp"):
            (root / sub).mkdir(parents=True)
        self.root = root
        fake = self.tmp / "host" / "oxa"
        fake.parent.mkdir()
        fake.write_text(FAKE_OXA)
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
        self.binary = fake
        self.logs = self.tmp / "trial" / "agent"
        self.logs.mkdir(parents=True)
        # Point the "container" paths at the temp directory.
        patches = {
            "REMOTE_BINARY": str(root / "bin/oxa"), "REMOTE_UPLOAD": str(root / "tmp/upload"),
            "REMOTE_LOGS": str(root / "logs/agent"),
        }
        patches["REMOTE_USAGE_LOG"] = f"{patches['REMOTE_LOGS']}/oxa-usage.jsonl"
        patches["REMOTE_STDOUT"] = f"{patches['REMOTE_LOGS']}/oxa.jsonl"
        patches["REMOTE_STDERR"] = f"{patches['REMOTE_LOGS']}/oxa.stderr"
        patches["REMOTE_EXIT"] = f"{patches['REMOTE_LOGS']}/oxa.exit"
        for key, value in patches.items():
            old = getattr(core, key)
            setattr(core, key, value)
            self.addCleanup(setattr, core, key, old)
        self.argv_file = self.tmp / "argv.txt"
        self.env = {"HOME": str(root / "home"), "PATH": f"{root / 'bin'}:{os.environ['PATH']}", "FAKE_ARGV_FILE": str(self.argv_file),
                    "FAKE_USAGE_FIXTURE": str(HERE / "fixtures/usage.jsonl")}

    def _agent(self, module, **kwargs):
        opts = dict(oxa_binary=str(self.binary), proxy_url="https://bench-proxy.example.invalid:8089",
                    agent_timeout_sec=900, model_name="opencode/kimi-k3")
        opts.update(kwargs)
        return module.OxaAgent(self.logs, **opts)

    def _run(self, module, exit_code=0, startup_failure=False, instruction="fix the bug", **kwargs):
        agent, ctx = self._agent(module, **kwargs), self.Context()
        env = FakeEnvironment(self.root, dict(
            self.env,
            FAKE_OXA_EXIT=str(exit_code),
            FAKE_OXA_STARTUP_FAILURE="1" if startup_failure else "0",
        ))
        install = agent.install if module is self.harbor else agent.setup

        async def go():
            await install(env)
            await agent.run(instruction, env, ctx)

        asyncio.run(go())
        return agent, ctx, env

    def argv(self):
        return self.argv_file.read_text().splitlines()

    def test_adapter_builds_the_expected_argv(self):
        for module in (self.harbor, self.pier):
            with self.subTest(module=module.__name__):
                self._run(module)
                argv = self.argv()
                self.assertEqual(argv[:4], ["-p", "--yolo", "--mode", "json"])
                for flag in ("--no-context-files", "--no-mcp", "--no-web-search", "--no-session"):
                    self.assertIn(flag, argv)
                self.assertEqual(argv[argv.index("--model") + 1], "opencode/kimi-k3")
                self.assertEqual(argv[argv.index("--usage-log") + 1], core.REMOTE_USAGE_LOG)
                self.assertEqual(argv[-1], "fix the bug")

    def test_adapter_ends_option_parsing_before_a_leading_dash_instruction(self):
        instruction = "- Implement the requested change"
        for module in (self.harbor, self.pier):
            with self.subTest(module=module.__name__):
                self._run(module, instruction=instruction)
                self.assertEqual(self.argv()[-2:], ["--", instruction])

    def test_adapter_passes_max_time_below_the_task_timeout(self):
        for module in (self.harbor, self.pier):
            with self.subTest(module=module.__name__):
                self._run(module)
                self.assertEqual(self.argv()[self.argv().index("--max-time") + 1], "780")
                self._run(module, margin_sec=300)
                self.assertEqual(self.argv()[self.argv().index("--max-time") + 1], "600")
        with self.assertRaises(core.BenchError):
            core.resolve_max_time(100, 120)

    def test_adapter_margin_is_configurable_from_the_environment(self):
        agent = self.harbor.OxaAgent(self.logs, oxa_binary="x", extra_env={
            "OXA_BENCH_PROXY_URL": "https://bench-proxy.example.invalid:8089", "OXA_BENCH_AGENT_TIMEOUT_SEC": "1000",
            "OXA_BENCH_MARGIN_SEC": "250"})
        self.assertEqual(core.resolve_max_time(agent._timeout, agent._margin), 750)

    def test_usage_log_is_collected_and_parsed(self):
        for module in (self.harbor, self.pier):
            with self.subTest(module=module.__name__):
                for name in core.COLLECTED:
                    (self.logs / name).unlink(missing_ok=True)
                _, ctx, _ = self._run(module)
                for name in core.COLLECTED:
                    self.assertTrue((self.logs / name).is_file(), name)
                details = usage_details_oxa.parse_usage_log(str(self.logs / "oxa-usage.jsonl"))
                self.assertEqual(ctx.n_input_tokens, details["input"])
                self.assertEqual(ctx.n_output_tokens, details["output"])
                last = core.result_record((self.logs / "oxa.stderr").read_text())
                self.assertEqual(last["turns"], 3)

    def test_container_gets_the_stub_key_and_the_proxy_never_the_real_one(self):
        for module in (self.harbor, self.pier):
            with self.subTest(module=module.__name__):
                self._run(module)
                home = self.root / "home"
                secrets = home / ".config/oxc/llm/cloud-secrets.json"
                self.assertEqual(json.loads(secrets.read_text()), {"opencode": core.STUB_KEY})
                self.assertEqual(stat.S_IMODE(secrets.stat().st_mode), 0o600)
                settings = json.loads((home / ".oxa/agent/settings.json").read_text())
                self.assertEqual(settings["oxa"]["cloud"]["opencode"]["base_url"], "https://bench-proxy.example.invalid:8089")
                self.assertTrue(settings["oxa"]["engine"]["inProcessCloud"])
        with self.assertRaises(core.BenchError):
            core.check_proxy_url("https://opencode.ai/inference/go")
        with self.assertRaises(core.BenchError):
            core.check_proxy_url("http://user:pw@172.17.0.1:1")

    def test_proxy_url_matches_the_native_transport_policy(self):
        self.assertEqual(
            core.check_proxy_url("https://bench-proxy.example.invalid:8089"),
            "https://bench-proxy.example.invalid:8089",
        )
        self.assertEqual(core.check_proxy_url("http://127.0.0.1:8089"), "http://127.0.0.1:8089")
        for url in (
            "http://172.17.0.1:8089",
            "http://bench-proxy.example.invalid:8089",
            "https://bench-proxy.example.invalid:8089?token=stub",
            "https://bench-proxy.example.invalid:8089?",
            "https://bench-proxy.example.invalid:8089/#fragment",
            "https://bench-proxy.example.invalid:8089/#",
            "https://bench-proxy.example.invalid:not-a-port",
        ):
            with self.subTest(url=url), self.assertRaises(core.BenchError):
                core.check_proxy_url(url)

    def test_only_scorable_exit_codes_pass_through(self):
        for code, ok in ((0, True), (1, True), (130, True), (3, False), (5, False), (8, False)):
            with self.subTest(code=code):
                if ok:
                    self._run(self.harbor, exit_code=code)
                else:
                    with self.assertRaises(RuntimeError):
                        self._run(self.harbor, exit_code=code)
                self.assertEqual((self.root / "logs/agent/oxa.exit").read_text().strip(), str(code))

    def test_exit_one_before_a_model_call_is_an_infrastructure_failure(self):
        for module in (self.harbor, self.pier):
            with self.subTest(module=module.__name__):
                with self.assertRaises(RuntimeError):
                    self._run(module, exit_code=1, startup_failure=True)
                self.assertFalse((self.root / "logs/agent/oxa-usage.jsonl").exists())

    def test_home_cwd_override_is_opt_in(self):
        self._run(self.harbor)
        self.assertEqual(Path(str(self.argv_file) + ".env").read_text().strip(), "0")
        self._run(self.harbor, allow_home_cwd=True)
        self.assertEqual(Path(str(self.argv_file) + ".env").read_text().strip(), "1")

    def test_pier_allowlist_names_only_the_proxy_host(self):
        agent = self._agent(self.pier)
        self.assertEqual(agent.network_allowlist().domains, ["bench-proxy.example.invalid"])

    def test_trial_prefix_is_added_to_the_base_url_when_asked(self):
        agent = self._agent(self.harbor, proxy_trial_prefix=True)
        self.assertEqual(agent._base_url(), "https://bench-proxy.example.invalid:8089/trial")


class InstallScriptTests(unittest.TestCase):
    SCRIPT = HERE / "install-oxa.sh"

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.art = self.tmp / "artifact"
        self.art.mkdir()
        (self.art / "oxa").write_bytes(b"not a real binary\n")
        digest = core.sha256_file(self.art / "oxa")
        (self.art / "oxa.sha256").write_text(f"{digest}  oxa\n")

    def install(self, *args, env=None):
        return subprocess.run(["sh", str(self.SCRIPT), *args], capture_output=True, text=True,
                              env=dict(os.environ, **(env or {})))

    def test_install_script_verifies_the_checksum(self):
        dest = self.tmp / "out"
        ok = self.install(str(self.art), str(dest))
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertEqual(ok.stdout.strip(), f"{dest}/oxa")
        self.assertTrue(os.access(dest / "oxa", os.X_OK))
        self.assertEqual(core.verify_binary(str(dest / "oxa")), core.sha256_file(dest / "oxa"))

        (self.art / "oxa").write_bytes(b"tampered\n")
        bad = self.install(str(self.art), str(self.tmp / "out2"))
        self.assertEqual(bad.returncode, 3)
        self.assertIn("checksum mismatch", bad.stderr)
        self.assertFalse((self.tmp / "out2/oxa").exists())
        with self.assertRaises(core.BenchError):
            core.verify_binary(str(self.art / "oxa"))

    def test_install_script_needs_a_checksum_and_refuses_http(self):
        (self.art / "oxa.sha256").unlink()
        self.assertEqual(self.install(str(self.art), str(self.tmp / "o")).returncode, 3)
        env = {"OXA_SHA256": core.sha256_file(self.art / "oxa")}
        self.assertEqual(self.install(str(self.art), str(self.tmp / "o"), env=env).returncode, 0)
        self.assertEqual(self.install("http://example.invalid/oxa").returncode, 2)


if __name__ == "__main__":
    unittest.main()
