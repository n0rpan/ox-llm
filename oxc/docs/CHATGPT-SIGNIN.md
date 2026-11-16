# ChatGPT sign-in: the Codex route (experimental)

oxc's ChatGPT subscription provider (`oxc/src/oxc/runtime/cloud_chatgpt.py`, issue #252) signs in
through the **Codex sign-in route**: the Codex CLI's public OAuth client identifier against
`auth.openai.com`, with the resulting tokens used against the ChatGPT backend's Codex Responses
endpoint. Other open-source harnesses ship the same route. **OpenAI does not document this route.**
It is tolerated, not supported, and may stop working without notice. It ships by owner decision
(issue #252). The provider stays opt-in: cloud must be on, and `oxc login chatgpt` must have been
run.

Every value below comes from source files fetched from the projects' repositories on 2026-11-15
and pinned to a commit. Line numbers refer to those commits:

- OpenCode `sst/opencode` @ `6c5ef96af4928524e1daa24d4a2c0d9c279cdefb`, file
  `packages/core/src/plugin/provider/openai.ts`. Its Codex methods are labelled "Codex browser
  (legacy)" and "Codex device code (legacy)".
- pi `badlogic/pi-mono` @ `463e17f8dc08a76a1e24e55db37f88de81b4b118`, files
  `packages/ai/src/auth/oauth/openai-codex.ts`, `packages/ai/src/api/openai-codex-responses.ts`
  and `packages/ai/catalog/providers/openai-codex.ts`.

Abbreviations: **OC** = https://github.com/sst/opencode/blob/6c5ef96af4928524e1daa24d4a2c0d9c279cdefb/packages/core/src/plugin/provider/openai.ts,
**PA** = https://github.com/badlogic/pi-mono/blob/463e17f8dc08a76a1e24e55db37f88de81b4b118/packages/ai/src/auth/oauth/openai-codex.ts,
**PR** = https://github.com/badlogic/pi-mono/blob/463e17f8dc08a76a1e24e55db37f88de81b4b118/packages/ai/src/api/openai-codex-responses.ts,
**PC** = https://github.com/badlogic/pi-mono/blob/463e17f8dc08a76a1e24e55db37f88de81b4b118/packages/ai/catalog/providers/openai-codex.ts.

| What | Value | OpenCode | pi | oxc constant |
|---|---|---|---|---|
| Client identifier | `app_EMoamEEZ73f0CkXaXp7hrann` | https://github.com/sst/opencode/blob/6c5ef96af4928524e1daa24d4a2c0d9c279cdefb/packages/core/src/plugin/provider/openai.ts#L18 | https://github.com/badlogic/pi-mono/blob/463e17f8dc08a76a1e24e55db37f88de81b4b118/packages/ai/src/auth/oauth/openai-codex.ts#L22 | `CLIENT_ID` |
| Issuer | `https://auth.openai.com` | OC#L19 | PA#L23 | `AUTH_URL` |
| Authorization endpoint | `https://auth.openai.com/oauth/authorize` | OC#L409-L410 | PA#L24 | `Flow.authorize_url` |
| Token endpoint (code and refresh) | `https://auth.openai.com/oauth/token` | OC#L345, OC#L359 | PA#L25 | `Flow.exchange`, `credentials` |
| Scopes | `openid profile email offline_access` | OC#L414 | PA#L34 | `SCOPE` |
| Redirect URI | `http://localhost:1455/auth/callback` | OC#L20 (port 1455; 1457 fallback at OC#L21), OC#L101 | PA#L26; port and path at PA#L368-L369 | `REDIRECT_PORT`, `REDIRECT_PATH` |
| PKCE | `S256` challenge | OC#L415-L416 | PA#L300-L301 | `Flow` |
| Extra authorize parameters | `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, `originator=<harness name>` | OC#L417-L420 (`opencode`) | PA#L303-L305 (`pi`, from PA#L290) | `Flow.authorize_url`, `ORIGINATOR` = `oxc`; `codex_cli_simplified_flow` is **not** sent (see below) |
| Code exchange body | form: `grant_type=authorization_code`, `code`, `redirect_uri`, `client_id`, `code_verifier` | OC#L344-L356 | PA#L151-L160 | `Flow.exchange` |
| Refresh body | form: `grant_type=refresh_token`, `refresh_token`, `client_id` (no scope) | OC#L358-L367 | PA#L170-L177 | `credentials` |
| Account identifier | claim `https://api.openai.com/auth` → `chatgpt_account_id` in the token | OC#L425-L437 | PA#L35, PA#L310-L315 | `AUTH_CLAIM`, `_record` |
| API base | `https://chatgpt.com/backend-api/codex`, requests to `<base>/responses` | OC#L25 | PR#L56, PR#L662-L668; PC#L49 | catalog `base_url` in `docs/cloud-models.json` |
| Request headers | `Authorization: Bearer <access>`, `chatgpt-account-id`, `OpenAI-Beta: responses=experimental`, `accept: text/event-stream`, `originator` | OC#L276-L280, OC#L320 | PR#L1680-L1695, PR#L293 | `cloud.connection` |
| Body constraints | `store: false`, `stream: true`, `instructions` from the system prompt (default `You are a helpful assistant.`), system prompt not in `input` | — | PR#L564, PR#L573-L578 | `adapt` |
| Large bodies | the backend rejects uncompressed bodies over about 1 MiB (OpenCode) and accepts zstd-compressed bodies (both) | OC#L272 | PR#L62 | not implemented (oxc sends plain JSON) |
| Models and limits | `gpt-6.1-sol`/`gpt-6.1-terra` (context 500000), `gpt-6-sol`/`gpt-6-luna` (context 272000), max output 128000; older models are refused for ChatGPT accounts | OC#L28-L29, OC#L302 | PC#L11-L26, PC#L54 | `docs/cloud-models.json` `chatgpt/*` |

**Deviation:** oxc leaves out the `codex_cli_simplified_flow=true` authorize parameter. Shipped
oxc files must not name a third-party coding client; the client-neutrality test
(`oxc/tests/test_core_client_neutral.py`, O-151) enforces this with no exceptions. Both harnesses
send the parameter, and its effect is not documented. If the sign-in page needs it, the owner has
to decide on O-151 before it is added. For the same reason, the code comments in
`cloud_chatgpt.py` cite only the OpenCode file (S1) with its line numbers; the pi references are
in this document. The backend URL lives only in the catalog.

Not taken over: OpenCode's and pi's device-code login (`/api/accounts/deviceauth/*`) and websocket
transport, plus OpenCode's `x-codex-beta-features` and session-affinity headers. oxc's headless
path is the paste fallback, `oxc login chatgpt --paste`, which pi also offers (PA#L364).

## The official program

OpenAI's official "Sign in with ChatGPT" program is a different route: dynamic client registration
for each user (OpenCode's newer `packages/core/src/plugin/provider/chatgpt.ts` at the same commit,
L22-L28: `dynamic_agent_client`, `/api/accounts/oauth/token`, scope `chatgpt.tokens.use.direct`).
That route does not yet work for an unlisted project like ox-llm, so oxc does not use it. It
replaces the Codex route once it is generally available (issue #247); it is not implemented here.

## Overrides and safety

`cloud.chatgpt.client_id` replaces the built-in identifier, and `cloud.chatgpt.auth_url` (https or
loopback only) replaces the issuer. The token file `chatgpt-auth.json` is owner-only. It sits next to
the cloud keys, is refreshed by one refresher at a time, is never logged and is hidden from the
agent sandbox. Requests have cost class `subscription`, and LAN guests are never served. Tests use
a fake auth server and a fake backend only.
