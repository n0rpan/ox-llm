# Lead web endpoint (experimental, #1030)

`oxa lead serve --enable-web` explicitly enables the owner web endpoint.
Without the flag it refuses; offline mode also refuses. Set the user environment
variable `OXA_LEAD_OWNER_LOGIN` to the exact owner Tailscale login. Project
settings cannot select or change the accepted identity.

The binary listens only on `lead-web/http.sock` below the user agent directory,
inside an owner-only 0700 directory, with a 0600 socket. It accepts exactly one
`Tailscale-User-Login` header matching that login. Missing, duplicated and other
identities are refused. No login, request headers, environment or credentials
are rendered. Request size, time and concurrent connections are bounded.

Run Tailscale Serve on a dedicated HTTPS port, for example 9443, targeting this
socket with `tailscale serve --bg --https=9443 unix:<absolute-socket-path>`.
Serve binds the tailnet interface, terminates TLS, strips client identity headers
and inserts its authenticated identity. Keep ports 443 and 9119 untouched. Do
not use Funnel or a TCP forwarder: only Serve's HTTP proxy supplies identity.
The private Unix socket prevents a direct TCP request from spoofing that header.
The local owner and tailscaled are trusted; other local accounts cannot open it.

Stop the binary before removing its socket for a restart. Startup preserves an
existing socket rather than risking replacement of another live instance.
Do not place the socket below a project checkout or share its private directory.

This first transport slice returns an authenticated landing page only. Board,
lanes, merge queue, release checklist, ask cards, SSE and lead chat follow in
separate small changes using the runner's shared types. This endpoint currently
has no model, GitHub mutation, worker launch or owner-decision write path.
