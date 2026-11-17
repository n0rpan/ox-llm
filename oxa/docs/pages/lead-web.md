# Lead web access scaffold

The read-only lead renderer and this access boundary are preparation for a later
`oxa lead serve` integration. No listener or production identity adapter is installed
yet. The owner must read the authentication boundary before merge or live exposure.
Chat and owner-decision mutations are separate later work.

Only the user settings file may configure `oxa.lead.serve`; a project's settings
cannot enable it or choose its listener or identity policy. It is disabled by
default. The default identity mode is `local`, the bind is `127.0.0.1`, and the
default port is `8743`. There are no deployment-specific port or identity rules.

Local mode requires independently verified local OS ownership, not merely a
loopback address. Tailscale mode requires an authenticated request-bound proxy
identity, its matching forwarded login, and an exact `allowed_identities` match.
An untrusted `Tailscale-User-Login` header and loopback peer alone prove nothing.
No provider is enabled implicitly; the shipped unverified adapter always refuses.

After a reviewed listener and trusted proxy adapter are available, configure the
user layer for your own identity and chosen ports, leaving it off until ready:

```json
{"oxa":{"lead":{"serve":{"enabled":false,"identity":"tailscale",
  "bind":"127.0.0.1","port":8743,
  "allowed_identities":["YOUR_LOGIN_IDENTITY"]}}}}
```

Operator guidance for Tailscale Serve (not an oxa command), using ports you selected:

```text
tailscale serve --bg --https=<tailnet-port> http://127.0.0.1:<local-port>
```

Choose a dedicated tailnet port whose ACL excludes shared or guest devices. This
is an operator step, not an action performed by oxa. Never assume the header is
trusted simply because Serve exists. Do not expose the backend directly or bypass
the verified proxy adapter. No account, tailnet, service or ACL is hard-coded.
