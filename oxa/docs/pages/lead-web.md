# Lead web access scaffold

Run `oxa lead serve` to serve the read-only board after enabling it in user settings.
`OXA_LEAD_BOARD_LOG` selects an absolute private board log; refused input stays unavailable.
The owner must read the final authentication composition before merge or live exposure.
Chat remains separate work. The one owner-decision mutation is the approval answer below.

## The page

Every page shares one chrome: a sticky top bar with a state dot and a light/dark
toggle, a fixed bottom navigation, and a floating chat button. The theme choice
is kept in the browser only, follows the system theme until you pick one, and
never leaves the device. From 900px wide the bottom bar becomes a left rail with
a sixth Chat entry, and the floating button hides.

- The Asks tab carries a red count badge only when asks are open. Zero or an
  unknown count shows no badge; the tab's accessible name says which it is.
- The active tab is marked with an accent bar and `aria-current=page`. On chat
  the floating button is absent, because the composer is there.
- Buttons, textareas, cards, chips and meters follow one token set (colour,
  type, space, shape) at the top of `lead_page/style.css`; light and dark come
  from the same tokens. Status colour always pairs with an icon or a word, and
  an unknown state never renders green.
- Pages no longer end with a "not connected yet" footer; what chat can do is
  shown on the chat page itself.

The home page's attention block reports the five stuck-work signals from the board
log's protected record: main required-check failures, no verified main merge for 45
minutes, over-age merge-queue membership at the exact PR head, workflow runs queued
over 15 minutes, and unanswered owner decisions. Each source is rendered from its own
observation: a source the record does not carry is never shown as healthy. The missing
sources collapse into one line ("Not in this sample: …") that names them, instead of one
bullet per source, and a routine healthy read renders no attention block at all. With
`owner_chat` enabled, open owner asks are additionally read live from the authorized
durable Lead Session on every render, with their original revisions, through the
registered owner-bound caller; an unauthorized or absent session leaves that source
unavailable rather than quietly empty.

## Last-known data, age and cause

Every gap on the page says why it is there. A missing observation names its source, a
cause, the age of the last good value and the next step, in one muted line. The cause
comes from a fixed allowlist (`BoardCause` for the board read, fixed text for the other
sources); a raw error, path or file content never reaches the page.

A transient board read failure keeps the last good board visible instead of blanking the
page. The board panel and the top banner say "Board is last-good", show the allow-listed
cause (for example "the protected Board observation changed while it was being read"),
the age of that value and a Retry link. The retention is bounded: the server keeps exactly
one last good sample and replaces it on the next successful read. The runner situation and
the worker registry are independent sources and are always read fresh, so a board hiccup
never blanks Workers or Asks. A first read that fails, with no good value yet, says so and
names the cause rather than inventing data.

Ages render as `<time data-at>` elements. The page re-labels them every 10 seconds on the
client with no request, so "4 min old" stays truthful on a quiet system; the timer pauses
while the tab is hidden. Nothing here adds polling, a model prompt or a new refusal.

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

### The list of running sessions

With `owner_chat` on, the chat page also lists every session of your user that is running
now: the lead TUI, an interactive `oxa`, a worker. Each row shows the first characters of the
id, the project directory's name (never the full path), the time of its last activity and a
**View** button. Choosing one attaches the page to it until the service restarts: the same
live transcript and the same message form as above (a message goes into that session's inbox,
like `oxa send`). The attached view also shows whether the session is running and working or
idle (from its newest message) and the prompt tokens of its newest request when it recorded
usage; the session does not record the model's window, so no share of the window is shown.

"Running" means that the process named in the session's lock file is alive; the page never
takes the lock, so looking cannot make a starting `oxa` see its session as locked. After a
crash the recycled process id of an unrelated process can briefly make a dead session look
running. A session that is not running cannot be chosen, and a message to one that stopped
waits in its inbox until the session runs again.

`oxa.lead.attachSessionId` stays the start-up default. When it is unset the page starts on the
dedicated Lead chat, or, with no dedicated session either, on the list alone. The **Back**
button returns to that start-up target. Everything the page shows here is visible only to the
verified owner on `/chat`; this includes the transcript of your other running sessions and
workers, masked and escaped as described above. A choice is not a setting: restart the service
and it starts on the default again.

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
loopback address, and a per-start client token. `oxa lead serve` prints
`http://127.0.0.1:<port>/?t=<token>` once; opening it sets an `oxa_lead` cookie
(HttpOnly, SameSite=Strict) and redirects to the same path without the query. Tools may send
the token as `Authorization: Bearer`; it is also in `<agent dir>/lead-client-token` (0600),
replaced on every start. A request without it gets 401. Tailscale mode is unchanged. Tailscale mode requires an authenticated request-bound proxy
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

## One observation, live workers and live refresh

Every page is rendered from the same observation the Lead loop uses: the protected
Board sample (board, alarms, GitHub App quota, thread holds and the quota deferrals the
owner's reserve recorded) and the runner situation named by `OXA_LEAD_SITUATION`, which
the pages did not read before. The server no longer makes its own partial read.

- **Board** lists the observed PRs with their next action, priority, lease and attempts
  today, under counted filter chips that narrow the list by next action. A wide screen
  shows a table; a phone shows the same rows as cards. A stale or
  absent board observation is named explicitly, never shown as empty healthy work.
- **Item** names the next action and the observed state: priority, lease, attempts today
  and the observed time. Title, checks, review state, findings and diff are not part of
  this observation, so each is shown as "Unavailable in this snapshot" rather than guessed.
- **Workers** shows the Lead session's live workers under "This host" first: name, lane
  (branch), model, a state chip and how old the worker's status file is (a Dead worker, a
  Stopped worker and a stale status show as such). The runner situation tasks are then
  grouped by their host, each with a state chip (Stall reported, Busy at observation, or
  Idle reported for a host with no running task); within a host the reported stalls come
  first and the observation order is otherwise kept. The parent is the session of the Lead
  owner binding, never a session list. No workers recorded and no runner observation render
  the "Lane status unavailable" text; a corrupt registry renders "Live workers refused" and
  is left untouched.
- **Capacity** (home and Workers pages) shows the owner's user-only `oxa.lead.capacity`
  map next to observed use: one row per host as `running workers / configured capacity`,
  for example `host-a 2/6`. A host the owner did not configure shows an unknown capacity;
  without a runner observation the use is unknown, never zero. It also reports the Ready
  queue depth (eligible items the loop would propose first) and the stuck work the
  situation reports (stalled tasks, idle hosts, blocked task/host subjects). A board or
  situation that did not reach the page leaves the corresponding line explicitly
  unavailable, and an unreadable user settings file shows "Capacity unavailable". An
  invalid capacity entry is ignored and counted, never refused. This is display only:
  nothing schedules, refuses or starts work on it, and a project file cannot set it.
- **Usage** adds a line for each optional GitHub read the reserve deferred
  (`oxa.lead.github_reserve`), saying whether it waits for a window reset or for a first
  observation. Model plan windows still render only from an observation that reached the
  page; the provider-header retention that would feed them is not part of this version.
- **Live refresh.** With the route installed (always for `oxa lead serve`) the document
  advertises it and the page opens `GET /events/live`, behind the same authorization as
  every other path. The server sends `retry: 5000` and a `lead-update` event at once and
  again only when a fingerprint of the observation changes (board time, situation
  sequence, ask revisions, worker states, capacity edits), polled once a second; a `: keep-alive`
  comment goes out every 15 seconds and the stream ends after five minutes so the
  client reconnects. The event carries the SHA-256 of the serving executable, so a
  changed digest after an update reloads the page. Streams use the existing
  16-connection pool and per-write deadline. The one-shot `/events` route and the
  polling fallback remain for listeners without the route.

## Transport: one request for a repeat page

The stylesheet and the script are served at URLs that name their bytes:
`/a/<first eight hex digits of SHA-256>/style.css` and `/a/<sha8>/live.js`. The
document links them there, so a repeat visit reuses the browser's copy without a
request (`Cache-Control: public, max-age=31536000, immutable`); a new build changes
the digest and therefore the URL, so a cached asset is never a stale one. These two
files are the only shared, cacheable answers: they hold no page data, identity or
session, and a page is still `no-store`.

A page or a `?fragment=1` answer carries an `ETag` of its exact bytes. A browser that
sends the same `ETag` back in `If-None-Match` gets `304 Not Modified` with no body and
the same CSP, referrer, `nosniff` and `no-store` headers. An unchanged page therefore
costs one revalidated request. The `ETag` is compared only after the request is
authorized: a refused request never learns whether a page changed, and gets no
validator. A `*` or a weak tag never answers `304` for a page, because the bytes, not
the page's identity, are what the client needs to hold. Each asset representation
(raw or gzip) has its own validator, so a `304` never names bytes the client does not
have.

Pages, fragments and assets are gzipped (`flate2`, level 6) when the request's
`Accept-Encoding` allows it and the result is smaller; the raw bytes are served
otherwise, and a body that would grow is never compressed. The compression happens
per response for a page and once at start for an asset. The one-shot `/events` route
and the live stream are not compressed.

Sizes measured against a running `oxa lead serve` on an owner machine with the
healthy fixture frame (`curl -s -o /dev/null -w '%{size_download}'`): Home is 5,421
bytes raw and 1,661 bytes with `Accept-Encoding: gzip`; the stylesheet 16,350 raw and
4,439 gzip; the script 5,581 raw and 1,955 gzip. A repeat visit fetches the document
only, so it carries about 1.7 KB instead of 27 KB.

## Synthetic fixture frames

Six synthetic frames under `oxa/oxa-cli/tests/fixtures/lead/` drive the real service in
`tests/lead_serve.rs`: `healthy`, `needs_you`, `alarm`, `stale`, `partial` and `first_run`.
Each file holds the native wire formats — one complete Reader sample record (board, panels,
landing, queued runs, App quota, deferrals) and one runner situation document — with
synthetic identities only. Timestamps sit at a synthetic reference epoch; the test rebases
them so the frame renders the age it declares (`age_secs`, `situation_age_secs`), which
keeps the files deterministic while the freshness states stay real.

The tests assert behaviour through the real routes: a healthy frame renders no attention
block; open decisions render as a request, not an alarm; an alarm names every firing
source; a stale board keeps its rows and offers a retry; a partial sample keeps its known
sources and names each gap with its cause; a first run renders explicit missing states and
never a fabricated row; a malformed replacement degrades explicitly and a fresh sample
recovers. The hosted screenshot job proposed for #2349 renders these same frames at phone
and desktop widths in light and dark, so every `lead_page/` change is reviewed by looking
at it.
