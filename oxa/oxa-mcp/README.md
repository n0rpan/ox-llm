# oxa-mcp

MCP client: stdio + streamable HTTP, OAuth (PKCE, dynamic registration), `mcp.json`
(user/project), exposure `codemode` (default) / `deferred` / `direct` / `hidden`, resource
tools, `/mcp`, `oxa mcp add/remove/list/login/logout`, `mcp.log`, `--no-mcp`.

| Module | Task | Provides |
|---|---|---|
| `types.rs` | architect | `ServerConfig`, `ServerKind`, `StdioConfig`, `HttpConfig`, `OAuthSettings`, `McpConfig`, `McpTool`, `CallToolResult`, `ToolAnnotations`, `ServerState`, `ServerStatus`, `ToolStatus`, `ConfigScope`, `AccessGate` |
| `transport.rs` | architect | `Transport`, `TokenSource`, `PipeTransport` (implemented, for tests) |
| `proto.rs` | E7 | `McpClient`, `rpc_request`, `rpc_notification`, `rpc_decode`, `Incoming`, `result_text` |
| `stdio.rs` | E7 | `StdioTransport` |
| `http.rs` | E8 | `HttpTransport`, `parse_sse_messages` |
| `config.rs` | E9 | `load_config`, `parse_server`, `expand_value`, `effective_exposure`, `add_server`, `remove_server`, `set_server_field`, `AddOptions`, `entry_from_add_options`, config paths (implemented) |
| `names.rs` | E9 | `mangle`, `tool_names`, `split_full_name`, `same_server`, `valid_server_name`, `pattern_matches` |
| `oauth.rs` | F5 | `TokenStore`, `StoredAuth`, `pkce_pair`, `discover`, `redirect_uri`, `authorization_url`, `parse_redirect`, `CallbackServer`, `LoginIo`, `login`, `OAuthTokens` |
| `manager.rs` | F6 | `McpManager` (`impl Hooks`), `ManagerOpts`, `McpToolAdapter`, `ResourceTool`, `servers_section`, `tool_spec`, `McpLog` |

`oxa-cli/src/mcp_cmd.rs` (E9) and `oxa-interactive/src/selectors/mcp.rs` (F7) are the two
front ends; both use only the API above.

## Cross-task contracts

- **Transport** (`transport.rs`) is the seam between E7 (`McpClient`), E8 (HTTP) and F5
  (`TokenSource`). E7 tests run over `PipeTransport`; E8 tests run against a local test server.
- **W3**: only `Direct` tools reach the frozen tool list, read once per epoch; a late server is
  offered at the next epoch. `servers_section` is a pure function of the configuration.
  Exposure changes are saved at once but change declared tools only at the next epoch.
- **Names**: `mcp__<server>__<tool>` via `names::tool_names` (collision = hash suffix on all
  colliding tools). Everything else in the workspace treats the full name as opaque.
- **Permissions**: the manager asks `ManagerOpts::gate` before starting / reaching a server;
  each call is checked by the core from `McpToolAdapter::access` (`Access::Mcp`). Class from
  annotations: `readOnlyHint: true` -> `Read`, else `Write` (workspace mode asks for MCP write
  tools).
- **Editing `mcp.json`** keeps unrelated content and order (`serde_json` `preserve_order`),
  atomically.
- **Auth**: credentials are keyed by server name + URL in `<agent dir>/mcp-auth.json` (0600).
  A 401 without a usable token is `CODE_NEEDS_AUTH` -> `ServerState::NeedsAuth`.
- The manager does not know `oxa_config::Dirs`: the embedder passes `agent_dir`, `home`, `cwd`
  in `ManagerOpts` (test isolation).
- Codemode wiring (in `oxa-cli`): `catalog()` and `namespaces()` feed
  `oxa_codemode::CodemodeHooks`; `wants_codemode()` / `wants_tool_search()` decide activation.

## Acceptance

- E7: initialize, tools/list, call, progress resets timeout, resources; stop = stdin close,
  SIGTERM, SIGKILL of the group.
- E8: POST + SSE responses against a local test server; `sse` type rejected; headers and env
  expansion.
- E9: user/project replace and partial override rules; `!command` values; name mangling +
  collision hash; add/remove keep unrelated content.
- F5: PKCE; dynamic registration + configured client; callback server; token file 0600;
  refresh on 401; pasted redirect URL.
- F6: background connect; 10 s first-prompt wait; exposure modes; `mcp_servers` section;
  `mcp.log` rotation; late server = next epoch.

## Used by

`oxa-cli` (wiring, `mcp_cmd.rs`), `oxa-interactive` (`selectors/mcp.rs`).
