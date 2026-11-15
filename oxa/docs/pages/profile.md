# Tools and context handling

The behavior below applies when `local` is selected, as in the [quickstart](quickstart.md).
Use `OXA_PROFILE=local` for a shell or set `oxa.profile` to `local` in user settings.
This version defaults to `pi` without explicit selection: bash has no implicit timeout,
thinking uses `fixed`, and an unpaged read uses the ordinary 2000-line/16-KB cap.
Explicit settings override the selected defaults.

- The seven built-in tools are `read`, `bash`, `edit`, `write`, `grep`, `find` and `ls`.
  `edit` declares `path`, `oldText` and `newText`; several changes use several calls.
- A `bash` call without a timeout stops after 120 seconds; `oxa.bash.defaultTimeout`
  changes that value. Trust, permissions and the sandbox apply to every call.
- Tool output sent to the model is bounded. The first unpaged `read` is at most 8 KiB;
  larger results include a continuation hint. See [tools.md](tools.md) for paging and caps.
- `oxa.thinking.policy` defaults to `auto`: thinking is on for the first reply after
  a user prompt and off for tool follow-ups, summaries and background work. `fixed`
  applies the session's thinking level to every request; a level of `off` always wins.
- Compaction targets a short summary and can prepare one while idle. If summary requests
  fail repeatedly, a mechanical record preserves user messages and files touched.
  See [compaction.md](compaction.md) for the controls and context limits.
- Draft and tool-result prefill require an endpoint that advertises prefill support.
  Without that capability those optimizations do nothing.

Sessions, MCP, all extension hooks, skills, JSON and RPC modes remain supported.
See [settings.md](settings.md), [security.md](security.md) and [rpc.md](rpc.md).
