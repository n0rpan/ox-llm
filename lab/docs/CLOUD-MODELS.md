# Cloud models through the controller (Wave D)

The controller (`local-ai`, the oxc gateway) can proxy a model to a cloud provider over the
same loopback OpenAI v1 endpoint as the local models. One client, one model id, no second
provider type in the client.

Cloud is **off by default**. While it is off, remote catalog entries are not listed and a
request naming one gets the ordinary `model_not_found` reply. `cloud.enabled: true` in
`runtime.json` is the only switch.

## Catalog

A remote model is one entry under `cloud.models`:

```json
{
  "cloud": {
    "enabled": true,
    "daily_budget_usd": 2.0,
    "models": {
      "openrouter/qwen-example": {
        "kind": "remote",
        "provider": "openrouter",
        "base_url": "https://openrouter.ai/api/v1",
        "upstream_model": "vendor/model-id",
        "context_window": 131072,
        "max_output": 16384,
        "pricing": {"input_per_million": 0.4, "output_per_million": 1.6}
      }
    }
  }
}
```

- `kind` is `remote` (or the explicit `remote/openrouter` / `remote/anthropic`); `provider`
  selects the wire format. The first implementation is an OpenAI-compatible OpenRouter
  upstream.
- `base_url`, `upstream_model`, `context_window` and `max_output` are required. The display
  `name` is optional.
- `pricing` is USD per million tokens and is used for the ledger cost and the daily budget.
- Remote ids are listed in `GET /v1/models` and `GET /_local/models` while cloud is on.
  Each row carries `remote: true`, `provider`, `context_length`, `max_output_tokens` and a
  `capabilities` block (`remote`, `echo`, `cache_hints`, `cancel`, `prefill`,
  `context_window`).

## Key file

Keys live once in a controller secrets file, outside the repository:

```json
{"openrouter": {"api_key": "..."}, "anthropic": {"api_key": "..."}}
```

The default path is `cloud-secrets.json` next to `runtime.json`
(`~/.config/local-ai/llm/cloud-secrets.json`); `cloud.secrets_file` may name another absolute
path outside the repository. The file must be a regular file owned by the user with mode
`0600` (no group or other bits). It is read at request time. The controller never writes the
key to the request ledger, a status reply, an error or a log line; an upstream error that
echoes the key is redacted before it reaches the client.

## Privacy and budget

- Every OpenRouter request is forced to
  `"provider": {"zdr": true, "data_collection": "deny"}`. A client's own `provider` object may
  add routing preferences but cannot turn either requirement off.
- `OXC_OFFLINE=1` (also accepted: `LOCAL_AI_OFFLINE=1`) hard-refuses cloud models with `403`
  and code `offline`, before any DNS or socket call. Remote models are not listed while
  offline.
- `cloud.daily_budget_usd` is a positive USD cap per local day. A missing or non-positive cap
  fails closed (`budget_not_configured`). Once the sum of today's `cost_usd` ledger rows
  reaches the cap, requests get `429` with code `budget_exceeded`.
- Every remote request writes one ledger row with `remote: true`, `remote_provider`, the
  upstream model id, token usage when the provider reports it and `cost_usd`. The prompt and
  reply text are never written.

## Tests

`lab/tests/test_cloud_remote.py` runs a mock OpenRouter server on loopback: routing and the
forced privacy prefs, the key boundary, cloud-off-by-default, the offline refusal, the budget
cap, streamed usage metering and ledger rows. No test reads a real key or calls a real cloud
API.

## OpenAI v1 normalisation

The controller applies oxc's existing OpenAI v1 rules to a remote reply as well as a local
one:

- `reasoning_effort` is the standard switch: `none`/`minimal` = off, `low`/`medium`/`high` =
  on, `xhigh` = `high`; a non-string is a 400 with `param: "reasoning_effort"`. The local
  `chat_template_kwargs.enable_thinking` switch maps the same way when no effort is sent.
  Local-only fields (`chat_template_kwargs`, `cache_prompt`, slot/sampling controls) never
  reach a remote upstream.
- A stream asks for the final usage chunk (`stream_options.include_usage`), and tool calls
  keep the OpenAI streaming shape: same `index`, id and name, arguments always a JSON string.
- A provider `reasoning` field is exposed as `reasoning_content`. `usage` always carries
  `prompt_tokens_details.cached_tokens` and `completion_tokens_details.reasoning_tokens`;
  provider values win, missing values are completed.
- The reply's `model` is the catalog id the client asked for, not the upstream id.

## Per-model capabilities and oxa

Every `/v1/models` row and every `model_limits[id]` entry in `/_local/status` carries a
capability block with the client-neutral names:

| field | meaning |
|---|---|
| `remote` | `true` when the model runs at a provider through the controller |
| `remote_provider` | `openrouter` / `anthropic`, else `null` |
| `echo` | the engine echoes a cached reply (local Rust engine) |
| `cache_hints` | local prefix hints (snapshot, hint clear, cache warming) are useful |
| `cancel` | a running request may be cancelled by closing the socket |
| `prefill` | zero-token prefill (`max_tokens: 0`) is available |
| `context_window` | the model's context window; the context size a client should use |

The older local fields (`prefill_only`, `snapshot_hint`, `hint_clear`, `preserve_thinking`,
`park`, `max_context`) remain next to them for existing clients.

oxa reads these per model before its first request:

- it takes `context_window` (or the row's `cap`) as the model's context size;
- for a remote model it does not send the local `X-Oxe-*` hints, `cache_prompt`,
  `chat_template_kwargs`/`preserve_thinking`, prefix snapshots or cache-warming requests; it
  sends the standard `reasoning_effort` instead and skips local cache-miss accounting;
- under `--offline` / `OXA_OFFLINE` it refuses a model whose `remote` capability is true with
  the usual two-line offline error.

The interactive status bar shows the location: `local` in the theme's green, or
`cloud · openrouter` / `cloud · anthropic` in the accent colour; the start banner shows the
same segment.

## Anthropic Messages (D6)

An entry of kind `remote/anthropic` uses the native Messages API through the official
`anthropic` Python SDK, not OpenAI compatibility mode. The three model ids are
`anthropic/claude-haiku-5-5`, `anthropic/claude-sonnet-5-5` and
`anthropic/claude-opus-5-5`; put the key in the same owner-only secrets file under `anthropic`.

```json
"cloud": {"enabled": true, "daily_budget_usd": 2.0, "models": {
  "anthropic/claude-haiku-5-5": {"kind": "remote/anthropic", "provider": "anthropic",
    "base_url": "https://api.anthropic.com", "upstream_model": "claude-haiku-5-5",
    "context_window": 200000, "max_output": 8192,
    "pricing": {"input_per_million": 0.8, "output_per_million": 4.0}},
  "anthropic/claude-sonnet-5-5": {"kind": "remote/anthropic", "provider": "anthropic",
    "base_url": "https://api.anthropic.com", "upstream_model": "claude-sonnet-5-5",
    "context_window": 200000, "max_output": 16000,
    "pricing": {"input_per_million": 3.0, "output_per_million": 15.0}},
  "anthropic/claude-opus-5-5": {"kind": "remote/anthropic", "provider": "anthropic",
    "base_url": "https://api.anthropic.com", "upstream_model": "claude-opus-5-5",
    "context_window": 200000, "max_output": 32000,
    "pricing": {"input_per_million": 15.0, "output_per_million": 75.0}}
}}
```

`base_url` is the SDK base (`https://api.anthropic.com`), not a `/v1/messages` URL.

Translation rules:

- OpenAI system/developer messages become the Messages `system` block; user/assistant text
  becomes text blocks; assistant `tool_calls` become `tool_use` blocks; tool results become
  `tool_result` blocks. `tools`, `tool_choice` (`auto` / `none` / `required` / named) and the
  output limit are translated; `max_tokens` is required by the upstream and falls back to the
  catalog `max_output`.
- `reasoning_effort` `low`/`medium`/`high`/`xhigh` maps to adaptive thinking
  (`thinking: {"type": "adaptive"}`) plus `output_config.effort`; `none`/`minimal` map to
  `thinking: {"type": "disabled"}`.
- `cache_control: {"type": "ephemeral"}` breakpoints go on the system block, the last tool
  definition and the latest history turn (the frozen prefix and the newest turn).
- Replies are translated back to OpenAI v1: text, `reasoning_content` from thinking blocks,
  `tool_calls` with JSON-string arguments, usage including cache reads, and `stop_reason`
  mapping (`end_turn`/`stop_sequence` -> `stop`, `max_tokens` -> `length`, `tool_use` ->
  `tool_calls`, `refusal` -> `content_filter`). Streamed events become OpenAI chat chunks and
  end with `data: [DONE]`.
- The same cloud-off default, `OXC_OFFLINE` refusal, secrets file, daily budget and ledger rows
  apply as for OpenRouter.

Install the SDK in the controller's Python environment with
`python3 -m pip install 'anthropic==0.125.0'` (or the package extra `snapdragon-local-ai-lab[cloud]`).
If it is missing, an Anthropic request answers `503` with `cloud_sdk_unavailable`; local models
are unaffected.

Tests: `lab/tests/test_cloud_anthropic.py` runs a mock Messages server on loopback (request
translation, tools, refusal, streaming tool calls, key redaction, budget and cloud-off). No test
reads a real key or calls the real API.
