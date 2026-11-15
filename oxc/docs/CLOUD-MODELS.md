# Cloud models through the controller (Wave D)

The controller (`oxc`, the oxc gateway) can proxy a model to a cloud provider over the
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

- `kind` is `remote` (or the explicit `remote/openrouter` / `remote/anthropic` / `remote/openai`); `provider`
  selects the wire format. The first implementation is an OpenAI-compatible OpenRouter
  upstream.
- `base_url`, `upstream_model`, `context_window` and `max_output` are required. The display
  `name` is optional.
- `pricing` is USD per million tokens and is used for the ledger cost and daily budget. Both input and output prices must be explicit. Unknown pricing refuses the request; an entry with no prices must explicitly declare `"free": true`. Cloud requests support one completion (`n: 1`).
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
(`~/.config/oxc/llm/cloud-secrets.json` for a non-installer setup). The cloud-only installer
writes its own path into `cloud.secrets_file`: `<config-dir>/cloud-secrets.json`. You can point
`cloud.secrets_file` at another absolute path outside the repository. The file must be a regular file owned by the user with mode
`0600` (no group or other bits). It is read at request time. The controller never writes the
key to the request ledger, a status reply, an error or a log line; an upstream error that
echoes the key is redacted before it reaches the client.

## Privacy and budget

- Every OpenRouter request is forced to
  `"provider": {"zdr": true, "data_collection": "deny"}`. Client routing preferences
  are not forwarded.
- `OXC_OFFLINE=1` (also accepted: `OXC_OFFLINE=1`) hard-refuses cloud models with `403`
  and code `offline`, before any DNS or socket call. Remote models are not listed while
  offline.
- `cloud.daily_budget_usd` is a positive USD cap per local day. A missing or non-positive cap
  fails closed (`budget_not_configured`). Admission atomically reserves the worst case
  (`catalog context_window + bounded output limit` at the model price) before forwarding, so concurrent
  requests cannot all spend the same budget; the actual cost settles after the ledger row is
  written. A request that would exceed `spent + reserved` gets `429` with code
  `budget_exceeded`.
- Missing usage charges the reserved worst case, never zero. An unreadable, corrupt or
  unwritable ledger refuses remote calls with `503` and code `cloud_ledger_unavailable` until
  it is repaired.
- Every remote request writes one ledger row with `remote: true`, `remote_provider`, the
  upstream model id, token usage when the provider reports it and a settled `cost_usd`. The
  prompt and reply text are never written. Every response path (streamed chunks, success
  bodies, errors and error bodies) is redacted for the upstream key before it reaches the
  client or the ledger.

## Tests

`oxc/tests/test_cloud_remote.py` runs a mock OpenRouter server on loopback: routing and the
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
| `remote_provider` | `openrouter` / `anthropic` / `openai`, else `null` |
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
- `reasoning_effort` always maps to adaptive thinking (`thinking: {"type": "adaptive"}`) plus
  `output_config.effort`; `none`/`minimal` map to the lowest effort. Current Claude models
  reject `thinking: {"type": "disabled"}` and `temperature`/`top_p`/`top_k`, so those are never
  sent. Forced `tool_choice` is rejected with a clear `400` on models that refuse it.
- Every assistant thinking block, including its `signature`, and every
  `redacted_thinking` block is kept and replayed unchanged on the following requests of the
  same conversation. The history stays append-only, so the prompt-cache prefix is stable.
- `cache_control: {"type": "ephemeral"}` breakpoints go on the system block, the last tool
  definition and the latest history turn (the frozen prefix and the newest turn).
- Replies are translated back to OpenAI v1: text, `reasoning_content` from thinking blocks,
  `tool_calls` with JSON-string arguments, usage including cache reads, and `stop_reason`
  mapping (`end_turn`/`stop_sequence` -> `stop`, `max_tokens` -> `length`, `tool_use` ->
  `tool_calls`, `refusal` -> `content_filter`). Streamed events become OpenAI chat chunks and
  end with `data: [DONE]`. A stream that fails after `message_start` still writes a ledger row
  with the reported input tokens and the reserved cost.
- The same cloud-off default, `OXC_OFFLINE` refusal, secrets file, daily budget and ledger rows
  apply as for OpenRouter.

Install the SDK in the controller's Python environment (the one that runs the installer, e.g. the
venv of the quickstart) with
`python -m pip install 'anthropic==0.125.0'` (or the package extra `snapdragon-local-ai-lab[cloud]`).
If it is missing, an Anthropic request answers `503` with `cloud_sdk_unavailable`; local models
are unaffected.

Tests: `oxc/tests/test_cloud_anthropic.py` runs a mock Messages server on loopback (request
translation, tools, refusal, streaming tool calls, key redaction, budget and cloud-off). No test
reads a real key or calls the real API.


## OpenAI API-key upstream (D8)

Use `kind: "remote/openai"` and put the platform API key under `openai` in the
same owner-only secrets file. The default base URL is `https://api.openai.com/v1`.
Cloud remains off by default and requires a daily budget; offline mode refuses it.
This route uses exact-spec HTTP, with no SDK dependency or automatic retries.

Set `upstream_model`, `context_window`, `max_output`, and `pricing` in each catalog
entry using the model's published limits and your account's prices. No prices or
model limits are supplied implicitly. The per-model capabilities disable echo,
cache hints, cancellation and prefill; oxa shows `cloud · openai` in its footer
and start banner.

OpenRouter and generic compatible requests always carry an output limit, bounded by
catalog `max_output` (4096 when omitted). Admission reserves the entire catalog input
window because text-byte estimates cannot bound tokenization or image costs.

An expired active Responses tool continuation returns a clear `409`; it is never
retried automatically. A fresh user turn can replay completed public tool history
after expiry or compaction, without attaching opaque reasoning from another prefix.
Unresolved tool calls still require the original continuation state.

The endpoint is selected by an explicit model contract:

- `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6.1-terra`, `gpt-6-sol` and `gpt-6-luna`
  use Responses, including when tools are present. Chat messages, function tools,
  tool outputs, streamed argument fragments, refusals and usage are translated
  to/from the gateway's Chat Completions interface.
- `gpt-4.1`, `gpt-4.1-mini`, `gpt-4.1-nano` (and their `2025-04-14` snapshots),
  `o3` (`2025-04-16`) and `o4-mini` (`2025-04-16`) use Chat Completions.
- Other ids are refused before forwarding. Adding an alias or snapshot requires
  reviewing its model/endpoint contract and extending the tests.

Both endpoints set `store: false` and bound output to the catalog's `max_output`.
Chat Completions uses `max_completion_tokens`, including reasoning tokens.
Sampling controls are removed from reasoning requests. For o3/o4-mini, thinking
`none`/`minimal` maps to `low`, and `xhigh` maps to `high`. For Responses,
`minimal` maps to `low`; `none` also maps to `low` for Astra and 6.1 Sol, which
require reasoning. Other supported efforts are preserved.

Responses continuations replay native output, including encrypted reasoning,
from bounded process memory (128 entries, 8 MiB, 30-minute expiry), scoped to the
upstream, credentials and full client history. Nothing is persisted or logged.
A tool continuation whose native state has expired returns
`409 cloud_continuation_unavailable`; start a new turn. A process restart also
expires this state. Client bodies cannot opt into stored Responses or substitute
a different endpoint. Missing usage and interrupted streams retain the shared
conservative reservation charge. OpenAI admission reserves the full catalog
context window at the input price plus the bounded output at the output price.

The contract follows the official [GPT-6 guidance](https://developers.openai.com/api/docs/guides/latest-model),
[reasoning replay guidance](https://developers.openai.com/api/docs/guides/reasoning),
and [Chat Completions reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create).
Tests in `oxc/tests/test_cloud_openai.py` exercise loopback mocks only.

OpenRouter settlement records `catalog_cost_usd` and, when supplied by upstream,
`reported_cost_usd` separately. The charged `cost_usd` is their maximum, so a more
expensive routed provider consumes the daily budget accurately. Only finite,
non-negative numeric `usage.cost` values from upstream are accepted; client fields
are not billing evidence. Repeated usage frames cannot reduce an earlier bill.
Admission reserves the full catalog input window and bounded output, with 25%
additional OpenRouter headroom. This cushion is not a bound on every provider's
price: configure conservative catalog rates for the allowed routes. A reported
bill can exceed the reservation; it is still charged in full and blocks subsequent
requests at the daily cap. Missing usage or a failed request retains at least the
reservation and any larger reported bill. No automatic paid retry is made.

Compatible upstream requests use an explicit chat-field allow-list. Client plugins,
fallback-model lists, transforms, service tiers and routing preferences are not forwarded.
OpenRouter receives only the mandatory privacy preferences. Tools are client function
definitions; provider-hosted paid tools are refused. Ordinary tool selection, reasoning,
sampling and structured-output controls remain supported, including omitted/null tools.
Anthropic translation and admission share one validated output limit, including `n_predict`,
and never exceed the catalog maximum. Invalid limits refuse before an upstream call.

## Prompt tiers, caching and public price refresh (#48)

A price can use an ordered tier list instead of a flat pair. Bounds are inclusive;
the final tier must omit `max_prompt_tokens` (or use `null`) so every prompt is priced.
For example, these are synthetic rates, in USD per million tokens:

```json
"pricing": {"tiers": [
  {"max_prompt_tokens": 100000, "input_per_million": 1, "output_per_million": 2,
   "cache_read_per_million": 0.1, "cache_write_per_million": 1.25},
  {"input_per_million": 5, "output_per_million": 10,
   "cache_read_per_million": 0.5, "cache_write_per_million": 6.25}
]}
```

Settlement selects the tier from actual total prompt tokens, including cache reads and
writes. It charges uncached input, cache reads and cache writes separately. Optional cache
rates also work on flat prices and can be inherited from the outer price object by tiers.
Without an explicit read rate, no read discount is assumed. For Anthropic (including
Anthropic IDs routed through OpenRouter), an unspecified write rate uses the documented
2x input bound for one-hour writes; configure an explicit rate for the selected cache policy.
Malformed/inconsistent usage retains the reservation. Tiny nonzero costs are not rounded
to free. [Anthropic cache pricing](https://platform.claude.com/docs/en/about-claude/pricing).

Admission retains the full catalog input window, using the highest known input/read/write
rate and highest output rate across all tiers, plus OpenRouter's routing cushion. This is
more conservative than a prompt-size estimate near a tier boundary and preserves the bound
for dense text and images. No cache hit is assumed before the provider reports it.

When cloud is enabled and online, the controller fetches the public
[OpenRouter model list](https://openrouter.ai/docs/api/api-reference/models/get-models)
at startup and once per day, without a key or authorization header. Enabling cloud later
is detected within a minute. Replies are bounded and do not follow redirects. Valid public
prices are cached atomically in `oxc/openrouter-prices.json` under the XDG state root;
the cache contains model prices only. Offline/cloud-off mode makes no refresh request.
Failures, malformed prices and disappearing models retain the last-known values, never a
zero fallback. Unknown prices still refuse. Fetched rates can raise configured prices;
explicit configured rates and tiers remain conservative floors. A missing public cache rate
does not erase a configured one. Public list prices can still differ from the chosen routed
provider's bill: #47's validated `usage.cost` remains an additional settlement floor.

## Experimental: ChatGPT subscription (`chatgpt/<model>`, opt-in)

> **Experimental and unsupported.** This route signs in with the public PKCE client of OpenAI's
> own command-line coding agent and calls the ChatGPT subscription Responses backend, the way
> several third-party agents do. OpenAI does not document it; it is tolerated and **may break
> without notice**. The official "Sign in with ChatGPT" program (#247) replaces it.

It is off by default. It needs `cloud.enabled: true`, catalog entries of kind
`remote/chatgpt` named `chatgpt/<model>`, and an explicit sign-in:

```sh
oxc login chatgpt                 # browser PKCE sign-in, redirect to http://localhost:1455
oxc login chatgpt --paste         # headless: open the URL anywhere, paste the redirected address
oxc login chatgpt --import ~/.codex/auth.json   # reuse an existing CLI login once (read only)
oxc logout chatgpt                # removes the token file
```

```json
"chatgpt/gpt-5": {"kind": "remote/chatgpt", "upstream_model": "gpt-5",
                  "base_url": "https://chatgpt.com/backend-api/codex",
                  "context_window": 272000, "max_output": 128000}
```

- Tokens live in `chatgpt-auth.json` next to `cloud-secrets.json` (mode 0600, never logged or
  shown, redacted from replies). oxc refreshes them itself, one refresher at a time. oxa's
  sandbox denies the file in every mode.
- Requests are always streamed upstream, never stored (`store: false`); system/developer
  messages become `instructions`; the output cap is not sent (the backend rejects it).
- Cost class `subscription`: ledger rows cost 0 USD but keep their token counts. The daily USD
  budget does not apply; optional caps `cloud.chatgpt.daily_requests` and
  `cloud.chatgpt.daily_tokens` do.
- `/_local/status` → `cloud.subscription` shows `signed_in` and the remaining allowance the
  backend reported (`used_percent`, `resets_at`, `window_minutes`).
- LAN-share guests never see or reach these models; offline mode refuses them.
