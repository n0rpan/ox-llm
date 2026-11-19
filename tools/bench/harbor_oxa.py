"""Harbor 0.22.0 custom agent: oxa as a headless installed agent.

    PYTHONPATH=tools/bench harbor run ... --agent-import-path harbor_oxa:OxaAgent \\
        -m opencode/kimi-k3 --ae OXA_BENCH_PROXY_URL=http://172.17.0.1:8089 \\
        --ae OXA_BENCH_BINARY=/path/to/oxa --ak agent_timeout_sec=3600

Settings come from agent kwargs (`--ak`) first, then the host environment or `--ae`:
`oxa_binary` / OXA_BENCH_BINARY, `oxa_sha256` / OXA_BENCH_SHA256, `proxy_url` / OXA_BENCH_PROXY_URL,
`agent_timeout_sec` / OXA_BENCH_AGENT_TIMEOUT_SEC (default 3600), `margin_sec` / OXA_BENCH_MARGIN_SEC
(default 120), `allow_home_cwd`, `proxy_trial_prefix`. See README.md.
"""

import os
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

import oxa_bench as core


def _truthy(value):
    return str(value).strip().lower() in ("1", "true", "yes", "on")


class OxaAgent(BaseInstalledAgent):
    def __init__(self, *args, oxa_binary=None, oxa_sha256=None, proxy_url=None, agent_timeout_sec=None,
                 margin_sec=None, allow_home_cwd=None, proxy_trial_prefix=None, **kwargs):
        super().__init__(*args, **kwargs)
        env = self._get_env
        self._binary = core.first(oxa_binary, env("OXA_BENCH_BINARY"))
        self._sha256 = core.first(oxa_sha256, env("OXA_BENCH_SHA256"))
        self._proxy_url = core.first(proxy_url, env("OXA_BENCH_PROXY_URL"))
        self._timeout = float(core.first(agent_timeout_sec, env("OXA_BENCH_AGENT_TIMEOUT_SEC"),
                                         core.DEFAULT_AGENT_TIMEOUT_SEC))
        self._margin = float(core.first(margin_sec, env("OXA_BENCH_MARGIN_SEC"), core.DEFAULT_MARGIN_SEC))
        self._allow_home = _truthy(core.first(allow_home_cwd, env("OXA_BENCH_ALLOW_HOME_CWD"), False))
        self._trial_prefix = _truthy(core.first(proxy_trial_prefix, env("OXA_BENCH_PROXY_TRIAL_PREFIX"), False))
        # Fail at construction, before any container starts.
        core.check_proxy_url(self._proxy_url)
        core.resolve_max_time(self._timeout, self._margin)

    @staticmethod
    def name() -> str:
        return "oxa"

    def get_version_command(self):
        return f"{core.REMOTE_BINARY} --version"

    def _base_url(self):
        trial = Path(self.logs_dir).parent.name if self._trial_prefix else None
        return core.base_url(self._proxy_url, trial)

    async def install(self, environment: BaseEnvironment) -> None:
        if not self._binary:
            raise core.BenchError("set OXA_BENCH_BINARY (host path of the portable oxa binary)")
        digest = core.verify_binary(self._binary, self._sha256)
        (Path(self.logs_dir) / "oxa-binary.sha256").write_text(f"{digest}\n", encoding="utf-8")
        await environment.upload_file(self._binary, core.REMOTE_UPLOAD)
        await self.exec_as_root(environment, command=core.install_command())
        await self.exec_as_agent(environment, command=core.configure_command(self._base_url()))

    @with_prompt_template
    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        model = self.model_name or core.first(self._get_env("OXA_BENCH_MODEL"), core.DEFAULT_MODEL)
        argv = core.build_argv(instruction, model, core.resolve_max_time(self._timeout, self._margin))
        try:
            await self.exec_as_agent(environment, command=core.run_script(argv),
                                     env=core.run_env(self._allow_home) or None)
        finally:
            await self._collect(environment)
            self.populate_context_post_run(context)

    async def _collect(self, environment):
        """Copy what the framework did not already sync from /logs/agent."""
        for name in core.missing_files(self.logs_dir):
            try:
                await environment.download_file(f"{core.REMOTE_LOGS}/{name}", Path(self.logs_dir) / name)
            except Exception as exc:  # a missing file is normal when oxa never started
                self.logger.debug("could not collect %s: %s", name, exc)

    def populate_context_post_run(self, context: AgentContext) -> None:
        details = core.usage_totals(os.fspath(self.logs_dir))
        if details and details["input"] is not None:
            context.n_input_tokens = details["input"]
            context.n_cache_tokens = details["cached"]
            context.n_output_tokens = details["output"]
