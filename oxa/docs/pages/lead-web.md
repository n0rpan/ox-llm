# Lead web access scaffold

Run `oxa lead serve` to serve the read-only board after enabling it in user settings.
`OXA_LEAD_BOARD_LOG` selects an absolute private board log; refused input stays unavailable.
The owner must read the final authentication composition before merge or live exposure.
Chat remains separate work. The one owner-decision mutation is the approval answer below.

The home page's attention block reports the five stuck-work signals from the board
log's protected record: main required-check failures, no verified main merge for 45
minutes, over-age merge-queue membership at the exact PR head, workflow runs queued
over 15 minutes, and unanswered owner decisions. Each source is rendered from its own
observation: a source the record does not carry is shown as explicitly unavailable,
never as healthy, and a routine healthy read renders no attention block at all. With
`owner_chat` enabled, open owner asks are additionally read live from the authorized
durable Lead Session on every render, with their original revisions, through the
registered owner-bound caller; an unauthorized or absent session leaves that source
unavailable rather than quietly empty.

Optional user setting `oxa.lead.serve.fixed_prompts` defaults to false.
It enables no-JS, same-origin/CSRF-protected fixed questions at zero model tokens.
Private situation data needs browser proof separate from tool UIDs; absent inputs stay unavailable.

## Attach to a running session

With `owner_chat` on, the chat page can show and talk to a session that is running now (for
example the lead you started in tmux) instead of a dedicated Lead session. Find the session id
in the `id` field of the first line of its file under the sessions directory (the file name ends with it), then set the user-only
`oxa.lead.attachSessionId` to it and restart the service. The service then needs no dedicated
`oxa.lead.sessionId`; if both are set, attach wins.

The `/chat` page shows the newest part of the session's transcript (user and assistant text, each
tool call with a short masked argument preview, short result previews) and whether the session is
running. It refreshes like the other pages, and the page says when older history is not shown.
The message form sends exactly like `oxa send <id> <text>`: the text goes into the session's
inbox as a steer: a busy session reads it after its running tool calls, an idle one starts a
turn with it, also if it is not running yet. The service takes
no session lock, opens no provider and spends no model tokens. A message is text for the
model, never an approval: answer approvals on the approvals page. The same identity, origin and
CSRF checks as before apply, the session file must stay private to your user, and a refused send
is written to the service journal with stage `recheck`, reason `binding` or `inbox`.

## Waiting for your approval

The verified owner can see "Waiting for your approval" on the board and home pages.
The panel shows each pending request's ID, session, directory basename, command,
rule reasons and age. The page masks credentials in the command and rule reasons.

Viewing and answering require owner chat. Enable the user-only
`oxa.lead.serve.owner_chat` with Tailscale and exactly one allowed identity.
Each answer is a
same-origin, CSRF-checked form POST to `/approvals` with the fields `id` and
`answer` (`once`, `session`, `always` or `deny`), then a redirect to `/`.

An answer is equivalent to `oxa send <session> --approve <id> <answer>`: the same
approval desk write, the same user-only save of `always` keys to
`oxa.approval.always` (never the project's settings), and the same informational
note to the session. An unknown or already answered request returns 404 and
changes nothing. A refused answer is written to the service journal like a
refused chat request, with stage `approval`.

Only the user settings file may configure `oxa.lead.serve`; a project's settings
cannot enable it or choose its listener or identity policy. It is disabled by
default. The default identity mode is `local`, the bind is `127.0.0.1`, and the
default port is `8743`. There are no deployment-specific port or identity rules.

Local mode requires independently verified local OS ownership, not merely a
loopback address. Tailscale mode requires an authenticated request-bound proxy
identity, its matching forwarded login, and an exact `allowed_identities` match.
Local browser authority includes the port and must match the configured numeric bind. Proxy
proof must bind the independently authenticated expected authority to the request;
never copy an untrusted Host header into that proof. An untrusted
`Tailscale-User-Login` header and loopback peer alone prove nothing.
Only a loopback `bind` is accepted: a wider address is a configuration error, and
the access check refuses any request whose peer is not loopback.
No provider is enabled implicitly.

The explicit proxy adapter uses user-only `proxy.uid` and `proxy.authority` fields
inside `oxa.lead.serve`. Authority is configured independently of HTTP headers;
the accepted loopback stream's kernel owner must match the configured UID before
its login header can supply identity. Trust applies to every process with that
UID, so choose a principal dedicated to trusted proxy workloads. The kernel fallback
UID cannot identify a proxy principal: it can represent unmapped owners.
This adapter is Linux-only; other platforms refuse before binding. Missing trust also refuses.
Configuration does not install a listener or deploy a proxy.

Consumer integration must call `proxy::pre_bind` before binding, then
`proxy::verify` on the actual accepted stream and this request's single login
header. Reject duplicate/ambiguous identity and Host headers. Pass the resulting
proof, unchanged login and raw request authority to the existing `read_only`
boundary. A verification error refuses; it never permits a raw-header fallback.
The owner must read this producer before any consumer listener or exposure.

Configure the user layer for your own identity and ports, leaving it off until ready:

```json
{"oxa":{"lead":{"serve":{"enabled":false,"identity":"tailscale",
  "bind":"127.0.0.1","port":8743,
  "allowed_identities":["YOUR_LOGIN_IDENTITY"],
  "proxy":{"uid":12345,"authority":"lead.example.invalid:9448"}}}}}
```

The proxy UID and external authority above are synthetic examples. Replace them
with your trusted proxy principal and the exact authority the browser will send.

Operator guidance for Tailscale Serve (not an oxa command), using ports you selected:

```text
tailscale serve --bg --https=<tailnet-port> http://127.0.0.1:<local-port>
```

Choose a dedicated tailnet port whose ACL excludes shared or guest devices. This
is an operator step, not an action performed by oxa. Never assume the header is
trusted simply because Serve exists. Do not expose the backend directly or bypass
the verified proxy adapter. No account, tailnet, service or ACL is hard-coded.

The service writes chat refusals (4xx/503) to standard error. A user systemd
deployment can read them with `journalctl --user -u oxa-lead.service`. Each
line contains the response status, a fixed stage and reason, and a suggested
remedy. It omits the owner identity, message, token counts, URLs, headers,
credentials and CSRF values.
