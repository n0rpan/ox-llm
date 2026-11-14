# Design: "Sign in with ChatGPT" for oxc (D7, future work)

Status: **design only**. Nothing in this document is implemented, and no client identity of
another application is used. Implementation waits until the ox-llm project is registered as a
participating app by the program's owner.

## Goal

Let a user choose a ChatGPT-plan-backed model in oxc without copying an API key, through the
program's own OAuth flow for participating apps. The model then appears as a normal remote
catalog entry with the existing cloud switch, budget, offline refusal, ledger and capability
block. oxa does not change: it still sees one loopback OpenAI v1 endpoint.

## What the program provides

The program decides the authorization and token endpoints, the registered client id, the
allowed redirect URIs and the per-app usage limits. oxc must use **only its own registered
client id and redirect URI**. It must never read, copy or reuse the client id, redirect URI,
tokens or user-agent of Codex, another CLI, another editor, or any other application. There is
no fallback to "borrow" a client identity, and no scraping of a browser session or local token
store.

The exact endpoints, scopes, token lifetime and limit fields are recorded here only after the
project is registered; the flow below is the integration shape, not a claim about the current
public offering.

## Flow

1. The user turns cloud on (`cloud.enabled: true`) and chooses to sign in. Cloud stays off by
   default; no sign-in starts at boot.
2. oxc creates a random `state` value and a PKCE `code_verifier`, keeps them in memory for one
   attempt, and opens the program's authorization URL with:
   - oxc's registered client id,
   - a loopback redirect URI registered for oxc (an ephemeral `127.0.0.1` port is acceptable
     only if the registration allows it; otherwise a fixed loopback port),
   - the minimum scopes the program requires,
   - the `state` and the PKCE challenge.
3. The system browser performs the sign-in. oxc never asks for the user's ChatGPT password and
   never embeds a web view.
4. The loopback listener accepts exactly one redirect, checks `state`, rejects a mismatched or
   replayed response, and exchanges the authorization code with the PKCE verifier.
5. oxc stores the refresh token and the short-lived access token in its owner-only secrets file
   (`0600`, outside the repository), in a provider block separate from API keys. It stores no
   password and no browser cookies.
6. Access tokens are refreshed at request time when near expiry. A refresh failure marks the
   provider unready and answers `503` with `cloud_credentials_unavailable`; it does not fall
   back to an API key or another client.
7. Sign-out removes the token block from the secrets file, clears the in-memory token and
   disables the provider's remote models without touching local models.

The flow is authorization-code + PKCE for a public client. No client secret is shipped in the
repository or the installed package.

## Model catalog and requests

A successful sign-in adds a `remote/chatgpt` provider block to the catalog:

```json
"cloud": {
  "models": {
    "chatgpt/<model>": {
      "kind": "remote/chatgpt",
      "base_url": "<program endpoint>",
      "upstream_model": "<model id>",
      "context_window": 0,
      "max_output": 0
    }
  }
}
```

The values are filled from the program's model metadata after registration. As with the other
remote kinds, every request carries the provider's required privacy/usage headers, is forced
through the controller's OpenAI v1 normalisation, and writes a ledger row with
`remote: true`, the provider, usage and cost/limit facts.

## Limits and display

The program reports per-app usage limits. oxc:

- reads them with the access token (never with another app's identity),
- exposes the usable remaining amount and reset time in `/_local/status` `cloud` (numbers and
  enum labels only, never a token),
- refuses a request with `429` and `plan_limit_reached` when the program says the limit is
  reached, before sending the prompt,
- keeps the local USD budget cap as a second, independent guard.

The interactive status bar keeps the D4b segment: `local` in green, or
`cloud · chatgpt` in the accent colour. The start banner shows the same segment.

## Safety rules

- Cloud is off by default; signing in does not turn it on.
- `OXC_OFFLINE=1` refuses every remote model, including a signed-in ChatGPT model.
- Tokens live only in the owner-only secrets file, are never logged, never returned by a status
  route and never written to the request ledger.
- A failed or expired sign-in never silently downgrades to another provider or reuses another
  app's credentials.
- Implementation is blocked on the project's registration and on the program's documented
  endpoints, scopes, redirect rules and limit fields. Until then, the API-key providers in
  [CLOUD-MODELS.md](CLOUD-MODELS.md) are the supported remote path.
