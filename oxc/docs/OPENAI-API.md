# The local OpenAI-compatible API

Every local model is served on one loopback address in the OpenAI wire format, so any
harness or client library that can talk to "an OpenAI-compatible server" can use it.
The coding agent oxa (`oxc code`) is set up for you. Short recipes for other clients are
in [OTHER-CLIENTS.md](OTHER-CLIENTS.md); this page is the full reference.

Tags: [M] measured here without a model (stub engine), [S] from the harness's own
documentation or source, not run here, [?] open.

## 1. Connection

| Setting | Value |
|---|---|
| Base URL | `http://127.0.0.1:8092/v1` |
| API key | none by default. Clients that insist on one can send any text (`local`). |
| Model id | `local-default`, or a full name from the table below |
| Context | Ornith and Qwen3.6: 131 072 tokens; Flash: 16 384 (prompt and reply together; `context_size` in `GET /_local/status`) |
| Requests at a time | one. Further requests wait in a queue for up to 120 s, then get `503` with `Retry-After: 5`. |

Port 8092 listens on `127.0.0.1` only; there is no setting to change that. Other devices reach the model only through the separate, opt-in LAN share (section 6).

**Port 8092 is the only supported client entry.** The engine ports behind it (8093 and 8094)
are internal: they skip the API key, the CORS rule, the queue and the ledger. The
controller starts the Rust engine with a random token that is new on every launch (a 0600
file in the user's runtime directory) and only the controller sends it. The engine also refuses
browser requests (any `Origin` header or a foreign `Host`). Do not point a client at 8093 or 8094.

Check it from a shell:

```
curl http://127.0.0.1:8092/v1/models
curl http://127.0.0.1:8092/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"model": "local-default", "messages": [{"role": "user", "content": "Hello"}]}'
```

With the official Python client:

```python
from openai import OpenAI
client = OpenAI(base_url="http://127.0.0.1:8092/v1", api_key="local")
reply = client.chat.completions.create(model="local-default",
                                       messages=[{"role": "user", "content": "Hello"}])
```

The first request after a pause loads the model (10 to 60 s). Listing models never loads
one.

## 2. Model ids

| Id | Model | Notes |
|---|---|---|
| `local-default` | the model selected in the panel (Ornith today) | follows the selection |
| `local-heavy` | Flash (`qwen3.8-flash-next-local`) | the "think hard" model; slower; listed only while Flash is qualified |
| `ornith-35b-a3b-local` | Ornith 1.5 35B A3B | |
| `qwen3.6-35b-a3b-local` | Qwen 3.6 35B A3B | |
| `qwen3.8-flash-next-local` | Qwen 3.8 Flash-Next | same as `local-heavy` |
| `qwen-local` | same as `local-default` | older name, kept for existing client configs |

- `GET /v1/models` lists them in this order. Alias rows carry `alias_of` (the full name
  they point at); `qwen-local` also carries `legacy: true`.
- A `provider/` prefix in the litellm style is accepted: `openai/ornith-35b-a3b-local`,
  `openai/local-default`.
- The reply's `model` field is the full name of the model that answered.
- An unknown id gets `404` with `code: "model_not_found"`. Some harnesses send a second,
  fixed model name for side requests (titles, summaries). Either configure that model to
  the same id, or set `"openai_api": {"unknown_model": "default"}` in `runtime.json` to
  serve unknown ids with the selected model.
- **Only one model is loaded at a time.** A request for another id unloads the current
  model and loads the other (10 to 60 s, and the cached conversation is gone). Give a
  harness one id for all its roles.
- More aliases: `"openai_api": {"model_aliases": {"local-fast": "qwen3.6-35b-a3b-local"}}`.
  A `null` target removes a built-in alias. An alias cannot replace a full name.

Aliases are fixed names. Choosing a model per request (routing, escalation to Flash) is a
separate piece of work in the controller and is not part of this table.

### Flash limits

Flash is qualified for temperature 0, at most 2048 output tokens and thinking off. For
generic clients the controller brings a request into that range instead of refusing it:
`max_completion_tokens` becomes `max_tokens`, a limit above 2048 becomes 2048, another
temperature becomes 0, `reasoning_effort` is dropped. Each change is counted in
`request_compat` of `GET /_local/status`. Not yet run against the real Flash engine [?].

## 3. What is supported (conformance table, v0.2.0)

Measured through the controller in front of `oxe-server --engine stub` (the Rust engine's
real request and reply code, no model) with the real `openai` Python SDK [M]: the contract
suite `tools/openai-contract/` runs every row marked [C] in CI, against the controller (queue
on) and against the engine directly. Status words: **supported**, **partial** (works with a
documented limit), **unsupported** (a clear `4xx`, never a silent wrong answer). One behaviour
for every client; nothing depends on who the client is. Reproduce: section 8.

| Endpoint | Feature | Status | Detail |
|---|---|---|---|
| `/v1/models` | list [C] | supported | each row also carries `context_length` and `max_output_tokens` (below) |
| `/v1/models/{id}` | retrieve [C] | supported | `404`, `code: "model_not_found"` for an unknown id |
| `/v1/chat/completions` | non-streaming, streaming (SSE, `[DONE]`) [C] | supported | |
| `/v1/chat/completions` | `usage` [C] | supported | always with `prompt_tokens_details.cached_tokens` and `completion_tokens_details.reasoning_tokens` (below) |
| `/v1/chat/completions` | `stream_options.include_usage` [C] | supported | final chunk: `choices: []` plus `usage` |
| `/v1/chat/completions` | `max_tokens`, `max_completion_tokens` [C] | supported | an explicit value is never clamped; `-1` = no limit |
| `/v1/chat/completions` | no length field [C] | supported | the model's output default (below), then `finish_reason: "length"` |
| `/v1/chat/completions` | `stop`, `seed`, `temperature`, `top_p` [C] | supported | default temperature is 0 when none is sent |
| `/v1/chat/completions` | `logprobs`, `top_logprobs` [C] | supported | non-streamed, and on the streamed delta chunks (v0.2.0 engine) |
| `/v1/chat/completions` | `tools`, `tool_choice` auto / none / required / named, `parallel_tool_calls` [C] | partial | calls come back as `tool_calls` (also streamed), `finish_reason: "tool_calls"`. `tool_choice` is a strong instruction, not enforced by sampling; a named function narrows the tool list to it |
| `/v1/chat/completions` | `system`, `developer` roles; a system message after the first turn; a trailing assistant message [C] | supported | a late system message becomes a `<system-reminder>` user turn |
| `/v1/chat/completions` | `reasoning_effort` [C] | supported | `none` / `minimal`: thinking off; `low` / `medium` / `high` (and `xhigh`): thinking on. This standard field selects on/off; the separate local `thinking_budget` caps reasoning tokens. Absent: the server default (off). An explicit `chat_template_kwargs.enable_thinking` wins. An unknown value is ignored with `X-Local-AI-Warning`. Flash ignores it (`X-Local-AI-Ignored`) |
| `/v1/chat/completions` | `response_format` `json_object`, `json_schema` (not strict) [C] | partial | best effort: an instruction (with the schema) is added to the end of the last user message, and a non-streamed reply is checked: a JSON value in prose or a code fence is cut out; anything still invalid is returned as written with `X-Local-AI-Warning`. Streamed replies are not checked |
| `/v1/chat/completions` | `response_format` `json_schema` with `strict: true` [C] | unsupported | `400`, `param: "response_format.json_schema.strict"`: no grammar-constrained sampling yet, so no guarantee is offered. Send `strict: false` or use a tool |
| `/v1/chat/completions` | `prompt_cache_key` [C] | supported | the session hint, the same as an `X-Session-Id` header with that value (queue lane, cache, replay); the header wins when both are sent |
| `/v1/chat/completions` | `n` > 1 [C] | unsupported | `400`, `param: "n"`: one choice per request. `n: 1` or absent is fine |
| `/v1/chat/completions` | `frequency_penalty`, `presence_penalty`, `logit_bias` [C] | supported | forwarded unchanged, so the controller behaves exactly like the engine port; the v0.2.0 Rust engine applies all three (logprobs come from the adjusted row). The engine reports them in `/props` and `/_local/status` `model_limits[id].capabilities.sampling` lists them once the model runs. Only when the running engine reports that it lacks one does a non-zero value get `X-Local-AI-Ignored: <fields>` |
| `/v1/chat/completions` | image, audio, file parts [C] | partial | replaced by a short text note; the models read text only (the engine alone answers `400`) |
| `/v1/chat/completions` | `store`, `metadata`, `user`, `service_tier`, `prediction`, `modalities`, `verbosity`, `safety_identifier` | partial | accepted and ignored |
| `/v1/chat/completions` | deprecated `functions` / `function_call` | unsupported | not translated; send `tools` |
| `/v1/completions` | non-streaming, streaming, `logprobs`, `echo`, `stop` [C] | supported | translated to the engine's native route |
| `/v1/completions` | several prompts in one request, `suffix` (fill-in-the-middle) | unsupported | `400` |
| `/v1/responses` | stateless subset [C] | partial | see "The Responses API" below |
| `/v1/responses` | `previous_response_id`, `conversation`, `background`, stored prompts [C] | unsupported | `400` with the field as `param`: nothing is stored |
| `/v1/embeddings`, `/v1/rerank` [C] | | unsupported | `404`, `code: "unsupported_endpoint"`. No embedding model is installed |
| `/v1/audio/*`, `/v1/images/*`, `/v1/moderations` [C] | | unsupported | `404`, `code: "unsupported_endpoint"` |
| any other `/v1/...` route (files, batches, ...) [C] | | unsupported | `404`, `code: "unknown_url"`, any method |
| errors [C] | `{"error": {"message", "type", "param", "code"}}` | supported | `code` is always a string: the controller's own (`model_not_found`, `unsupported_value`, ...), an engine error mapped (context overflow: `context_length_exceeded` with `param: "messages"`, the engine's message and `type: "exceed_context_size_error"` kept), else one per status (`invalid_request`, `not_found`, `service_unavailable`, ...) |
| headers [C] | `x-request-id` | supported | a new id on every reply |
| retries [C] | SDK timeout + retry; `Idempotency-Key` | supported | a waiter whose client left leaves the queue; a streamed request cut during prefill and re-sent identically is served from the replay buffer (section 5) |
| auth | optional API key | supported | off by default (section 5) |
| CORS | browser apps on another origin | supported, opt-in | off by default (section 5) |
| bind address | loopback only | fixed | |

**Output default.** A request with no `max_tokens` / `max_completion_tokens` gets the
model's `max_output_tokens`: by default `min(16384, context // 4)` (16 384 at a 64K or larger
context). Change it per model in runtime.json with the profile key `max_output_tokens` (or once
for all at the top level); `0` means no default (the reply may run to the context limit). The
older key `default_max_tokens` is still read. The value is published in each `/v1/models` row as
`max_output_tokens`, next to `context_length`. At 20 tokens/s, 16 384 tokens is about 14
minutes, so a client that sends no limit is bounded but can still wait long; send a limit for
side requests such as titles.

**Reasoning tokens.** The v0.2.0 Rust engine counts them (its number is passed through). For an
engine that does not, the controller counts
the streamed reasoning deltas (one per token) and, for a non-streamed reply, splits
`completion_tokens` by the reasoning share of the generated text (an estimate). An engine that
reports the number is passed through unchanged.

### The Responses API (`POST /v1/responses`)

A stateless subset, translated to a chat completion by the controller (Rust engine models;
Flash answers `404`). Enough for the SDKs' `responses.create` and agent loops that send the
whole conversation each turn.

- Request: `model`, `instructions`, `input` (a string, or message items with `input_text` /
  `output_text` parts, `function_call` + `function_call_output`, `custom_tool_call` +
  `custom_tool_call_output`; `reasoning` items are skipped), `tools` (`function`, and `custom`
  free-form tools, which the model sees as a function with one string parameter `input`),
  `tool_choice`, `parallel_tool_calls`, `max_output_tokens`, `temperature`, `top_p`, `stream`,
  `reasoning.effort` (the `reasoning_effort` mapping), `text.format` (the `response_format`
  rules), `prompt_cache_key`. Accepted and ignored: `store` (the reply says `"store": false`),
  `metadata` (echoed), `user`, `include`, `truncation`, `service_tier`.
- Reply: `output` items `reasoning` (when the model reasoned), `message` (`output_text`), then
  `function_call` / `custom_tool_call`. `status` is `completed`, or `incomplete` with
  `incomplete_details.reason: "max_output_tokens"`. `usage` has `input_tokens`,
  `input_tokens_details.cached_tokens`, `output_tokens`,
  `output_tokens_details.reasoning_tokens`.
- Stream: `response.created`, `response.in_progress`, `response.output_item.added`,
  `response.reasoning_text.delta`/`.done`, `response.content_part.added`/`.done`,
  `response.output_text.delta`/`.done`, `response.function_call_arguments.delta`/`.done`,
  `response.custom_tool_call_input.delta`/`.done`, `response.output_item.done`, then
  `response.completed` or `response.incomplete`; an engine error is an `error` event and
  `response.failed`. Every event has `sequence_number`. SSE comments (`: queued`) pass through.
- Not supported (`400`): `previous_response_id`, `conversation`, `background: true`, stored
  `prompt`, `item_reference` items, tool types other than `function` and `custom` (web search,
  file search, computer use, ...).

Unsupported endpoints are answered by the controller without loading a model. The status is
`404`, not `501`, because client libraries retry every 5xx.

Scope: the translations (`/v1/completions`, `/v1/responses`, `reasoning_effort`,
`response_format`), the leniency for images and late system messages, and harness learning
apply to the Rust engine models (Ornith and Qwen3.6 in
production). The llama.cpp fallback profiles are passed through to llama-server, which has
its own OpenAI routes. Flash accepts `/v1/chat/completions` only.

Extra fields in replies that OpenAI does not have: `timings` (llama.cpp's block, with
`cache_n` and `prompt_n`), and `reasoning_content` on a message when thinking is on.
Thinking is off unless the request sends `reasoning_effort` `low` or higher, or
`"chat_template_kwargs": {"enable_thinking": true}`.

### Local thinking-token budget (opt-in)

On a supporting local oxe model, send `thinking_budget` on a chat-completions request:

```json
{"model":"ornith-35b-a3b-local","messages":[{"role":"user","content":"Explain the fix."}],"max_tokens":2048,"thinking_budget":512,"chat_template_kwargs":{"enable_thinking":true}}
```

The non-negative integer counts **generated reasoning text tokens**, excluding the think
markers. At the cap the engine evaluates a real `</think>` token and continues generating
an answer or tool call within the original total output limit. It does not append a fabricated
answer or retry the request. Zero closes reasoning immediately. An omitted or null budget
preserves the existing behavior, and the field does not itself enable thinking.

`max_tokens` / `max_completion_tokens` remains the total ceiling, including reasoning and
the closing marker. The effective reasoning budget is clamped to reserve a closing marker
and at least one answer token, also within the context window. A positive total limit of one
is refused with 400; zero remains prefill-only. Explicit stops, EOS and cancellation still
apply, so a budget bounds reasoning rather than guaranteeing a correct or complete answer.
`usage.completion_tokens_details.reasoning_tokens` includes the closing marker and can be
one greater than the reasoning-text cap.

Discovery: `/_local/status` `model_limits[id].capabilities.thinking_budget` is true only for
a local Rust engine binary advertising this extension; old/unknown binaries report false and refuse a non-null budget before loading.
The loaded engine's `/props.thinking_budget` also checks that the template and tokenizer
have the required single-token think markers. Unsupported templates and invalid values
(negative, fractional, boolean or string) return 400. Cloud models, LAN-share requests and
non-chat endpoints refuse a non-null budget before inference; it is not a provider's
reasoning-budget parameter. No profile default is changed by this API extension. oxa can opt in with
`{"oxa":{"thinking":{"budget":512}}}`; it sends the field only when thinking is on and
the selected local engine advertises support.

Talking to the engine port directly (no controller) is for tests. The v0.2.0 engine has its own
output default, `/v1/models/{id}`, string error codes and the sampling controls; it has no
`/v1/completions` in the OpenAI shape yet (planned for v0.2.0, O-176; the controller translates
today), no `/v1/responses`, and
`n` > 1 and `x-request-id` are still open there (`tools/openai-contract/xfail.engine.txt`).

## 3a. Cloud models (opt-in)

Cloud models are off by default. When `cloud.enabled` is true in `runtime.json`, remote
catalog entries appear in `/v1/models` and are called through the same endpoint by model id.
The key lives in an owner-only controller secrets file and is never listed or logged; every
OpenRouter routing preferences (`provider.zdr`, `data_collection`) are sent only if the owner sets them;
`OXC_OFFLINE=1` refuses remote models; a per-day USD cap stops requests once reached. See
[CLOUD-MODELS.md](CLOUD-MODELS.md).

## 4. Harnesses

None of the harnesses in this section was run here; the settings are from their
documentation [S]. What was verified is the wire format they use (section 3).

What they need from the server:

| Harness | Calls | Needs beyond plain chat | Works |
|---|---|---|---|
| Aider | `/v1/chat/completions`, streaming, through litellm | nothing; edits are plain text, no tools | yes |
| Continue (VS Code, JetBrains) | chat, streaming; tools in agent mode; `/v1/completions` for autocomplete | fill-in-the-middle for good autocomplete | chat, edit, agent: yes. Autocomplete: not recommended |
| Cline, Roo Code | chat, streaming, `include_usage` | a large context: the system prompt alone is over 10 000 tokens | yes on Ornith (32K) |
| Zed | chat, streaming, tools | | yes |
| LibreChat | `/v1/models`, chat, streaming; a title request per conversation | reach the host from Docker | yes with host networking |
| Open WebUI | `/v1/models`, chat, streaming; title, tag and follow-up requests | reach the host from Docker; CORS only for "direct connections" | yes with host networking |
| openai SDKs, LangChain, LlamaIndex | chat, completions, `responses.create` | `json_schema` for structured output by default; `.parse()` sends `strict: true` | yes. Structured output: function calling, or `strict: false` (best effort) |
| Codex CLI | `/v1/responses` (stream, function and custom tools) | the Responses API | yes, through the stateless subset (section 3) [S] |
| Claude Code | Anthropic Messages API | another wire format | **no** |

Rules that hold for all of them:

- One model id for every role (main, weak, title, summary, autocomplete).
- Set the context window to the model's (131 072 for Ornith and Qwen3.6, 16 384 for Flash). A harness that assumes more
  sends prompts the server answers with `400` (`type: "exceed_context_size_error"`).
- Background requests (titles, tags, autocomplete) wait in the same queue as the answer
  the user is waiting for. Turn them off where the harness allows it.
- Do not put a date, time or random id at the *start* of a system prompt. The prefix cache
  reuses a prompt up to the first changed token (section 7).

### Aider

```
export OPENAI_API_BASE=http://127.0.0.1:8092/v1
export OPENAI_API_KEY=local
aider --model openai/ornith-35b-a3b-local --no-show-model-warnings
```

litellm strips the `openai/` prefix; the server also accepts it. Aider does not know the
model, so tell it the context size in `.aider.model.metadata.json`:

```json
{"openai/ornith-35b-a3b-local": {"max_input_tokens": 32768, "max_output_tokens": 8192,
  "input_cost_per_token": 0, "output_cost_per_token": 0, "litellm_provider": "openai", "mode": "chat"}}
```

Aider uses the main model as its weak model for unknown models; if you set `--weak-model`,
give it the same id.

### Continue

`~/.continue/config.yaml`:

```yaml
models:
  - name: Local Ornith
    provider: openai
    model: ornith-35b-a3b-local
    apiBase: http://127.0.0.1:8092/v1
    apiKey: local
    roles: [chat, edit, apply]
    capabilities: [tool_use]
    defaultCompletionOptions:
      contextLength: 32768
```

Leave the `autocomplete` role to another provider or off: it fires on every pause in
typing, each request waits behind the chat, and the server has no fill-in-the-middle.

### Cline and Roo Code

API provider "OpenAI Compatible"; base URL `http://127.0.0.1:8092/v1`; API key `local`;
model id `ornith-35b-a3b-local`; context window 32768; no image support; no computer use.
Their system prompt is long, so the first request on a fresh engine is slow once
(section 7) and the room left for the conversation is about 20 000 tokens.

### Zed

`settings.json`:

```json
"language_models": {
  "openai_compatible": {
    "Local AI": {
      "api_url": "http://127.0.0.1:8092/v1",
      "available_models": [{
        "name": "ornith-35b-a3b-local", "display_name": "Ornith (local)",
        "max_tokens": 32768, "max_output_tokens": 8192,
        "capabilities": {"tools": true, "images": false, "parallel_tool_calls": false, "prompt_cache_key": false}
      }]
    }
  }
}
```

Zed asks for the API key in its provider settings; any text works.

### LibreChat

`librechat.yaml`:

```yaml
endpoints:
  custom:
    - name: "Local AI"
      apiKey: "local"
      baseURL: "http://127.0.0.1:8092/v1"
      models: {default: ["local-default"], fetch: true}
      titleConvo: true
      titleModel: "current_model"
```

LibreChat calls the API from its server, not from the browser. In Docker the container
must share the host's network (`network_mode: host`), because the API listens on the
host's loopback address only.

### Open WebUI

Admin settings, Connections, OpenAI API: URL `http://127.0.0.1:8092/v1`, key `local`. In
Docker run it with `--network=host`. In Admin settings, Interface, set the task model to
"current model" or turn title, tag and follow-up generation off. Its document search uses
its own built-in embedding model, not this API. "Direct connections" (the browser calls the
API itself) need the origin in `cors_origins` (section 5).

### Client libraries

- LangChain: `ChatOpenAI(base_url=…, api_key="local", model="local-default")`. For
  structured output pass `method="function_calling"`; the default (`json_schema`) is
  answered with `400`.
- The `openai` client retries `503` on its own, which is the right behaviour while another
  request is running.

## 5. Optional settings

All off by default. They live under `"openai_api"` in `runtime.json`
(`~/.config/oxc/llm/runtime.json`), read on every request, no restart needed.

```json
"openai_api": {
  "api_key_file": "/path/to/oxc/llm/api-key",
  "cors_origins": ["http://localhost:3000"],
  "model_aliases": {"local-fast": "qwen3.6-35b-a3b-local"},
  "unknown_model": "default"
}
```

- **API key.** `api_key_file` (first line of the file; keep it 0600) or the environment
  variable `OXC_API_KEY` of the controller service. With a key set, every API route
  needs `Authorization: Bearer <key>` (`x-api-key` and `api-key` headers also work) and
  answers `401` with `code: "invalid_api_key"` otherwise. Not covered: `/_local/*` (the
  panel's control routes, which the controller answers itself) and `/health`. The built-in
  chat pages do not send a key, so they stop working while one is set. A key file that is
  configured but unusable (missing, unreadable, not an absolute path) closes the API rather
  than opening it, and a `runtime.json` that is briefly unreadable does not drop the key.
  The key is never passed on to an engine.
- **Scope.** The API key and CORS settings cover port 8092 only. The engine ports are
  internal (section 1): the engine needs the controller's per-launch token and refuses
  browser requests (engines older than v0.1.1 answered any local process and web page there;
  update them).
- **CORS.** Without `cors_origins` a browser page on another origin is refused (`403`), as
  before: any web page you visit could otherwise use the model. List exact origins
  (`scheme://host:port`). `"*"` is honoured only together with an API key. Allowed origins
  get preflight answers, `Access-Control-Allow-Origin` and private-network access. The
  control routes (`/_local/*`) stay same-origin whatever is configured.

### Long prompts and client timeouts (v0.1.3)

A long append at depth prefills for minutes on the CPU before the first token (measured: 28.5K
tokens at ~83K context ran > 600 s; at 24-34 tok/s that is ~950-1200 s). Nothing fails if every
hop waits long enough:

- **Engine.** A streamed request gets its HTTP head at once, then one SSE comment every 15 s
  until the first token: `: queued wait_s=12`, then `: prefill 12288/28500 361s` (prompt tokens
  done / total, seconds). SSE parsers ignore comments, so the data frames are unchanged. Engine flag
  `--sse-keepalive SECS` (0 = off).
- **Controller queue** (on by default since v0.2.0; `OXC_QUEUE=0` turns it off, and
  `OXC_REPLAY=0` the replay of a cut stream). A streamed request that waits for the slot gets
  `200` and the SSE head at once, then `: queued pos=2 wait_s=31` every 15 s (place in the queue,
  1 = next; whole seconds waited). One format for both (v0.2.0): `: queued [pos=<n>] wait_s=<s>`,
  key=value fields after the word `queued`, unknown keys ignored; the engine's line has no `pos`
  (its worker is not an ordered queue).
- **Controller.** Its socket read timeout to the engine is `LOCAL_LLM_BACKEND_READ_TIMEOUT`
  (3600 s, an idle timeout: the comments reset it). It passes the comments through; the request
  ledger's `ttfb_ms` still counts from the first data frame.
- **Client timeouts.** Some clients abort a request whose response head or next SSE data event
  is late (a request also waits behind another in the controller queue), and then retry, which
  starts a second prefill. Give such clients a timeout of 3600 s; per-client settings are in
  [OTHER-CLIENTS.md](OTHER-CLIENTS.md).
- **Other clients.** Use a streamed request, or a whole-request timeout above the longest
  prefill. A client that drops the request does not lose all of the work: the engine keeps the
  prefill progress (state cache, every 4096 tokens) and a retry resumes from it.

## 6. Bind address

`127.0.0.1` only, fixed in the controller; the engines behind it also bind to loopback, and
for the Rust engine the controller refuses profile arguments that would change that.
Consequences:

- Other machines cannot reach the API unless you turn on the opt-in LAN share
  ([LAN-SHARE.md](LAN-SHARE.md): Tailscale or LAN, its own API key, inference routes only),
  or use an SSH tunnel (`ssh -L 8092:127.0.0.1:8092 host`).
- Docker containers cannot reach it through `host.docker.internal`; use host networking.

## 7. Caching for a harness the controller does not know

The Rust engine stores every system-prompt-plus-tools prefix of at least 1024 tokens on
disk, for any client, keyed by the token sequence. Nothing has to be registered. Measured
with two made-up harnesses (3 900 and 2 100 prompt tokens) on the stub engine, which runs
the real cache code [M] (`tools/openai_api_check.py cache`):

| Request | Reused tokens | Evaluated tokens |
|---|---|---|
| A, first request ever | 0 | 3882 |
| A, next request in the session | 3840 | 71 |
| A, new session, engine still loaded | 3840 | 42 |
| B (another unknown harness), first request | 0 | 2103 |
| A again after B | 3840 | 38 |
| A, new session after an engine restart (idle unload) | 3840 | 39 |
| B after the restart | 2048 | 54 |
| A with a different *last* line in the system prompt | 3840 | 47 |
| A with a different *first* line in the system prompt | 0 | 3883 |

So an unknown harness pays for its prefix **once per engine version**: the first request
after the harness is first used, after an engine or model update, or after "Clear caches".
Every later request, session, harness switch and restart reuses 97 to 99 % of the prefix
without the controller knowing the harness. At the 124 tokens/s cold prefill rate used in
`HARNESSES.md` section 4.1, that one time is about 31 s for 3 900 tokens and about 90 s for
a Cline-sized prompt of 11 000 tokens [E].

### What learning adds

Learning does not make any request faster that the store did not already make fast. It
adds three things:

1. **Prepare.** The controller keeps the harness's system prompt and tool list, so it can
   rebuild the stored prefix on request (a button, or "Prepare all" after an engine
   update) instead of inside the user's first request. Measured [M]: first request after
   an emptied store without Prepare, 0 tokens reused and 3876 evaluated; with Prepare
   3840 reused and 36 evaluated, and Prepare itself evaluated 3886. The cost moves; it
   does not shrink.
2. **A chip.** The harness shows up in the panel with a name and a ready / needs warm-up
   state, like any other recorded client.
3. **A diagnosis.** A client whose system prompt changes on every request is shown as
   `stable: false`. That is the case caching cannot help, and the usual reason a harness
   feels slow.

### How it works

Off by default. Two ways to opt in:

- all unknown clients: `POST /_local/harness/learning {"enabled": true}` (writes
  `"harness_learning": {"enabled": true}` to `runtime.json`);
- one client: `POST /_local/harness/learn {"harness": "learned-1a2b3c4d"}` for a candidate
  from the list.

With learning off, an unknown client is only counted in memory by the controller: sizes,
request counts, a fingerprint, the product name from its `User-Agent`. No text, and the
controller writes nothing to disk. This is about the controller's registry. The engine's
own prefix store is separate and works for every client regardless of this setting: it
holds the evaluated state of the system-prompt-plus-tools tokens (and their token ids), in
the private cache directory, which is what makes the table above possible.

A client is learned when the same system prompt and tool list arrived `min_requests` times
(default 3), byte for byte, and they are at least `min_prefix_chars` long (default 3000;
shorter prefixes are below what the engine stores). Then the controller writes a primer:
the leading system messages and the tool schemas, nothing else. It is the O-9 rule
unchanged: never a user turn, tool result or reply; file mode 0600 in a 0700 directory
(`~/.local/state/oxc/harness-prefixes`).

- Name: `learned-<8 hex>`, a hash of the first 256 characters of the system prompt and the
  tool names. A project path or date further down the prompt does not change the name.
- Label: the `User-Agent` product name, changeable with `POST /_local/harness/label`.
- At most 4 prefixes per learned client (for example one per project directory), least
  recently used dropped first; 24 prefixes in the registry overall, as before.
- "Forget" deletes the primers. With `"ignore": true` the client is also never learned
  again until it is approved by name.
- Clients the registry already knows are unchanged: recorded from their first request.
- Rust engine models only, like the rest of the registry.

### Endpoints (for the panel)

Polling `GET` routes never load a model. `POST` bodies are JSON of at most 1024 bytes.
These routes are same-origin only and are not covered by the API key.

| Call | Body | Reply |
|---|---|---|
| `GET /_local/harnesses` | | `{"harnesses": {...}, "harness_learning": {...}, "request_compat": {...}}`; the same three keys are in `GET /_local/status` |
| `POST /_local/harness/learning` | `{"enabled": true, "min_requests": 3}` (`min_requests` optional, 1 to 100) | the settings now in force; `400` on a bad body |
| `POST /_local/harness/learn` | `{"harness": "learned-…"}` | `{"approved": "learned-…"}`; `404` if that client was not seen since the controller started |
| `POST /_local/harness/label` | `{"harness": "learned-…", "label": "Aider"}` | `{"labelled": true}`; `400` otherwise. Letters, digits, space and `._+-`, 40 characters. |
| `POST /_local/harness/prepare` | `{"harness": "learned-…"}` or `{"harness": "*"}` (every harness that is not warm), optional `"model"` | per prefix `cache_n`, `prompt_n`, `seconds`; `409` when there is nothing to prepare. Loads the model. |
| `POST /_local/harness/forget` | `{"harness": "learned-…", "ignore": true}`; no `harness` forgets all | `{"forgotten": …, "ignored": …}` |

`harnesses` (existing shape; learned clients add two fields):

```
"learned-1a2b3c4d": {"learned": true, "label": "Zed", "warm": 1, "last_used": 1794345000.1,
                     "prefixes": [{"key": "c713f2bb9235", "model": "ornith-35b-a3b-local", "warm": true,
                                   "system_chars": 9120, "tools": 18, "requests": 14,
                                   "last_used": 1794345000.1, "last_cache_n": 3840, "last_prompt_n": 52}]}
```

`harness_learning`:

```
{"enabled": false, "min_requests": 3, "min_prefix_chars": 3000, "persistent": true, "ignored": [],
 "candidates": [{"harness": "learned-9f8e7d6c", "agent": "Aider", "requests": 12,
                 "distinct_prefixes": 1, "consistent_requests": 12, "stable": true,
                 "system_chars": 5200, "tools": 0, "prefix_chars": 5200, "large_enough": true,
                 "approved": false, "ignored": false, "first_seen": …, "last_used": …,
                 "last_cache_n": 1280, "last_prompt_n": 96}]}
```

Suggested chips: a learned harness is a normal chip with its `label` (fall back to the
name). A candidate with `stable` and `large_enough` is a dashed chip "Unknown client
(Aider) · Learn"; with `stable: false` it reads "prompt changes every request, cannot be
cached" and has no button. Candidates are kept in memory only, so they are gone after a
controller restart until the client talks again.

## 8. Reproducing the checks

```
# the engine's real request code without a model (DEV windows only)
oxe-server --engine stub --model <any gguf of the model family> --alias ornith-35b-a3b-local --port 18193 &
# the controller in front of it, with model start replaced by a marker
PYTHONPATH=src python3 tests/openai_api_stack.py --backend-port 18193 --port 18292 &

python3 tools/openai_api_check.py audit --base-url http://127.0.0.1:18292 --model local-default --markdown
python3 tools/openai_api_check.py sdk   --base-url http://127.0.0.1:18292 --model local-default   # needs `pip install openai` in a venv
python3 tools/openai_api_check.py cache --server <oxe-server> --gguf <gguf>

# the OpenAI contract suite: the pinned openai SDK in a private venv, both modes (repo root)
tools/openai-contract/run.sh --mode both --server <oxe-server>
tools/openai-contract/run.sh --mode controller --fake   # no Rust build: over the Python fake engine

PYTHONPATH=src python3 -m unittest tests.test_openai_api
OXE_STUB_SERVER=<oxe-server> OXE_STUB_GGUF=<gguf> PYTHONPATH=src python3 -m unittest tests.test_openai_api.Live
```

`audit` also runs against the live controller (`--base-url http://127.0.0.1:8092`); it then
loads a model and sends about 45 short requests.

## 9. Open points

- Strict `response_format` and an enforced `tool_choice` need grammar-constrained sampling in
  the engine. Until then: best effort, and `strict: true` is refused.
- `/v1/responses` is stateless: no `previous_response_id`, no stored responses.
- Penalties, `logit_bias` and streamed `logprobs` need sampler work in the engine.
- `/v1/embeddings`: no embedding model is installed. A small one on its own port would be
  a separate service, not a job for the LLM engine.
- `local-heavy` through a generic client has not been run against the real Flash engine.
- The harness notes in section 4 are from documentation; none was run against this server.
- `User-Agent` labels are whatever the client sends; several harnesses share a library's
  default (`OpenAI/Python`), which is why the label can be changed.
