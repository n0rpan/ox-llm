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
