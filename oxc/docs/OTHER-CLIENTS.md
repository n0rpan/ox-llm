# Use ox-llm from other clients

`oxc code` and the widget's **Code** button start oxa. Any other client that speaks the OpenAI
chat completions API can use the same controller. ox-llm ships no config for other clients; this page
is the recipe. The full API reference is [OPENAI-API.md](OPENAI-API.md).

## The recipe

| Setting | Value |
|---|---|
| Base URL | `http://127.0.0.1:8092/v1` (loopback only) |
| API key | any non-empty string; the controller ignores it unless you configured a key |
| Model | an id from `GET /v1/models`, for example `local-default` (the model selected in the panel) |
| Session header (optional) | `X-Session-Id: <a stable id per conversation>` |
| Context window | 131 072 for Ornith and Qwen3.6, 16 384 for Flash ([OPENAI-API.md](OPENAI-API.md) section 4) |
| Timeouts | 3600 s for the response head and between stream events |

- **One model id for all roles.** Only one model is loaded at a time; a request for another id
  unloads the current model. If a client sends a second, fixed model name for side requests (titles,
  summaries), give it the same id, or see `unknown_model` in [OPENAI-API.md](OPENAI-API.md).
- **`X-Session-Id`** is optional. With it, the controller tells the engine which conversation a
  request belongs to, which helps cache reuse when several conversations alternate. Without it,
  caching still works on exact prefixes.
- **Temperature.** Leave it unset for agent work: the controller then uses 0, which keeps
  speculative decoding on. A non-zero temperature switches speculation off.
- **Thinking.** The local profiles run with thinking off; declare the model as non-reasoning.
- **Timeouts.** A long prompt can prefill for minutes, and a request may wait behind another in the
  queue. A client that aborts and retries starts a second prefill.
- **Stable prefix.** Keep volatile text (dates, git status) out of system prompts and tool lists;
  each change is a new stored prefix to build.
- Do not point a client at the engine ports behind the controller; they need a per-launch token.

## Examples

These are recipes, not tested configs for every client version.

curl:

```sh
curl http://127.0.0.1:8092/v1/chat/completions -H 'Content-Type: application/json' \
  -H 'X-Session-Id: demo-1' -d '{"model": "local-default", "messages": [{"role": "user", "content": "Hello"}]}'
```

OpenCode (the v2 `providers` schema, last used with OpenCode 2.2.6 and 2.2.7): merge a provider into
your own `opencode.json`, then run `opencode -m ox-llm/local-default`. OpenCode only resets its
`chunkTimeout` on SSE data events, not on the keepalive comments, so keep both timeouts high.

```json
{"providers": {"ox-llm": {"package": "@opencode/ai/providers/openai-compatible",
  "settings": {"baseURL": "http://127.0.0.1:8092/v1", "apiKey": "local",
               "headerTimeout": 3600000, "chunkTimeout": 3600000},
  "models": {"local-default": {"type": "chat", "capabilities": {"tools": true, "reasoning": {"supported": false}},
                               "limit": {"context": 131072, "output": 4096}}}}},
 "agents": {"title": {"disabled": true}}}
```

pi: add a provider to `~/.pi/agent/models.json`, then run
`pi --provider ox-llm --model local-default`:

```json
{"providers": {"ox-llm": {"baseUrl": "http://127.0.0.1:8092/v1", "api": "openai-completions",
  "apiKey": "local", "models": [{"id": "local-default", "reasoning": false, "input": ["text"],
                                 "contextWindow": 131072, "maxTokens": 4096}]}}}
```

Other OpenAI-compatible clients (Aider, Continue, Cline, Zed, Open WebUI, LibreChat): see
[OPENAI-API.md](OPENAI-API.md) section 4.

Earlier releases wrote OpenCode config for you; v0.2.0 does not, and it never changes your own
OpenCode config. See the migration note in the [README](../../README.md#migrating-from-opencode).
Measurements of how OpenCode, pi and dsh use the cache are kept as history in
[HARNESSES.md](HARNESSES.md).
