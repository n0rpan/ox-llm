# oxa-web

The `web_search` tool (O-169): a provider abstraction with one provider, Brave Search (LLM
Context endpoint first, web search endpoint as the fallback for plans without LLM Context).
Layer 2, like `oxa-mcp`; the second and only other crate with an HTTPS client (the same `ureq`
stack).

| Module | Provides |
|---|---|
| `lib.rs` | constants: env names (`BRAVE_SEARCH_API_KEY`, `OXA_WEB_SEARCH`, `OXA_BRAVE_BASE_URL`, `OXA_TEST_ISOLATION`), settings (`oxa.webSearch.enabled` / `.provider` / `.maxTokens`), `--no-web-search` |
| `key.rs` | `ApiKey` (redacted `Debug`/`Display`), `KeyInputs`, `KeyStatus`, `KeyOrigin`, `load_key`, `real_home`, `remove_key_from_process_env` |
| `tool.rs` | `SearchProvider` (the seam), `Query`, `parse_query`, `WebSearchTool` (`impl Tool`) |
| `brave.rs` | `Brave` (`impl SearchProvider`), `BraveOpts`, `BraveShared`, `Endpoint`, `FALLBACK_NOTICE` — the only connect site |
| `shape.rs` | `Found`, `Budgets`, `shape`, `strip_html` |
| `hooks.rs` | `WebSearchHooks` (`impl Hooks`), `WebConfig`, `enabled`, `disabled_by_env`, `/web-search` status command |

## Contracts

- **W3**: the tool is part of the frozen prefix. `WebSearchHooks::tools` is read once per
  epoch and declares the tool only when a key is configured and offline mode is off. A key
  added mid-session is used from the next epoch. Same schema under both profiles (strings and
  integers only).
- **W4**: the result is a pure function of the provider's answer: header line, numbered sources
  (`[n] title`, URL, provider date, snippets) in the provider's order, per-snippet / per-source
  / total byte budgets, `(n more sources omitted)`, `No results.`. Every error text is fixed and
  lives in `oxa_tools::descriptions::web_search` (README rule 4).
- **The key** never reaches logs, notices, events, sessions, exports, errors or the wire.
  `BRAVE_SEARCH_API_KEY` wins over `<agent dir>/brave-search.key` (raw key, trailing newline
  trimmed; refused when group/world-readable, a symlink, not a regular file or empty). `oxa`'s
  `main` removes the variable from the process environment, so no child process inherits it.
- **Brave**: 30 s per request, no redirects, at most 3 attempts on 429 / 5xx with bounded
  exponential backoff (`Retry-After` / `X-RateLimit-Reset` honoured, each wait at most 10 s),
  requests serialised per process, cancellable. 401/403 = the key error. HTTP 400
  `OPTION_NOT_IN_PLAN` on LLM Context = web search for the rest of the process, said once
  (`Brave plan lacks LLM Context; using web search`) and in `/web-search`.
- **Permissions**: plan and workspace allow the call without a prompt (a read-only tool;
  O-167(b): tools get the network by default); ask mode asks `Access::Net { host }`. Offline,
  the tool is not declared and a call is refused with the offline error.
- **Tests (O-170)**: mock servers on loopback only. `OXA_BRAVE_BASE_URL` is honoured only for a
  loopback URL; with `OXA_TEST_ISOLATION=1` (set by `oxa_fake::TestEnv::envs`) the provider
  refuses the real host and a key file under the real home directory.
