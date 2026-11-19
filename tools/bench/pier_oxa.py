"""Pier 0.3.1 custom agent: oxa, same behaviour as harbor_oxa.py.

    PYTHONPATH=tools/bench pier run -p <task> --agent-import-path pier_oxa:OxaAgent \\
        --model opencode/kimi-k3 --agent-env OXA_BENCH_PROXY_URL=http://172.17.0.1:8089 \\
        --agent-env OXA_BENCH_BINARY=/path/to/oxa --agent-kwarg agent_timeout_sec=3600

Pier tasks may run with `allow_internet = false`: `network_allowlist()` names only the proxy host
(Pier allowlists take a host, no port). The binary is uploaded in `setup()`, not baked into the
image, so no Dockerfile step is needed.
"""

import os
from pathlib import Path

from pier.agents.base import BaseAgent
from pier.environments.base import BaseEnvironment
from pier.models.agent.context import AgentContext
from pier.models.agent.network import NetworkAllowlist

import oxa_bench as core


def _truthy(value):
    return str(value).strip().lower() in ("1", "true", "yes", "on")


class OxaAgent(BaseAgent):
    def __init__(self, logs_dir, *args, oxa_binary=None, oxa_sha256=None, proxy_url=None,
                 agent_timeout_sec=None, margin_sec=None, allow_home_cwd=None, proxy_trial_prefix=None,
                 extra_env=None, prompt_template_path=None, version=None, **kwargs):
        super().__init__(logs_dir, *args, **kwargs)
        self._extra_env = dict(extra_env or {})
        self._version = version

        def env(key):
            return self._extra_env.get(key, os.environ.get(key))

        self._binary = core.first(oxa_binary, env("OXA_BENCH_BINARY"))
        self._sha256 = core.first(oxa_sha256, env("OXA_BENCH_SHA256"))
        self._proxy_url = core.first(proxy_url, env("OXA_BENCH_PROXY_URL"))
        self._timeout = float(core.first(agent_timeout_sec, env("OXA_BENCH_AGENT_TIMEOUT_SEC"),
                                         core.DEFAULT_AGENT_TIMEOUT_SEC))
        self._margin = float(core.first(margin_sec, env("OXA_BENCH_MARGIN_SEC"), core.DEFAULT_MARGIN_SEC))
        self._allow_home = _truthy(core.first(allow_home_cwd, env("OXA_BENCH_ALLOW_HOME_CWD"), False))
        self._trial_prefix = _truthy(core.first(proxy_trial_prefix, env("OXA_BENCH_PROXY_TRIAL_PREFIX"), False))
        self._model_env = env("OXA_BENCH_MODEL")
        core.check_proxy_url(self._proxy_url)
        core.resolve_max_time(self._timeout, self._margin)

    @staticmethod
    def name() -> str:
        return "oxa"

    def version(self):
        return self._version

    def network_allowlist(self) -> NetworkAllowlist:
        return NetworkAllowlist(domains=[core.proxy_host(self._proxy_url)])

    def _base_url(self):
        trial = Path(self.logs_dir).parent.name if self._trial_prefix else None
        return core.base_url(self._proxy_url, trial)

    async def _exec(self, environment, command, user=None, env=None):
        result = await environment.exec(command=f"set -o pipefail; {command}", user=user,
                                        env=environment.agent_process_env(env))
        if result.return_code != 0:
            raise RuntimeError(f"command failed (exit {result.return_code}): {command[:200]}\n"
                               f"stderr: {(result.stderr or '')[-1000:]}")
        return result

    async def setup(self, environment: BaseEnvironment) -> None:
        if not self._binary:
            raise core.BenchError("set OXA_BENCH_BINARY (host path of the portable oxa binary)")
        digest = core.verify_binary(self._binary, self._sha256)
        Path(self.logs_dir).mkdir(parents=True, exist_ok=True)
        (Path(self.logs_dir) / "oxa-binary.sha256").write_text(f"{digest}\n", encoding="utf-8")
        await environment.upload_file(self._binary, core.REMOTE_UPLOAD)
        await self._exec(environment, core.install_command(), user="root")
        await self._exec(environment, core.configure_command(self._base_url()))
        if self._version is None:
            result = await environment.exec(command=f"{core.REMOTE_BINARY} --version")
            if result.return_code == 0 and result.stdout:
                self._version = result.stdout.strip().splitlines()[-1]

    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        model = self.model_name or core.first(self._model_env, core.DEFAULT_MODEL)
        argv = core.build_argv(instruction, model, core.resolve_max_time(self._timeout, self._margin))
        try:
            await self._exec(environment, core.run_script(argv), env=core.run_env(self._allow_home) or None)
        finally:
            for name in core.missing_files(self.logs_dir):
                try:
                    await environment.download_file(f"{core.REMOTE_LOGS}/{name}", Path(self.logs_dir) / name)
                except Exception as exc:
                    self.logger.debug("could not collect %s: %s", name, exc)
            self.populate_context_post_run(context)

    def populate_context_post_run(self, context: AgentContext) -> None:
        details = core.usage_totals(os.fspath(self.logs_dir))
        if details and details["input"] is not None:
            context.n_input_tokens = details["input"]
            context.n_cache_tokens = details["cached"]
            context.n_output_tokens = details["output"]
