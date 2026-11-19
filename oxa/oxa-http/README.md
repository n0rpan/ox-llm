# oxa-http

The one place for HTTP code that several oxa crates need.

- `sse`: `Framer`, a byte-wise Server-Sent Events framer (std only). It yields data events and comment lines; it decodes nothing and has no limits, so callers bound it with `pending_line` and `pending_data`.
- `server`: pure framing for hand-written servers (std only): `read_head`, `split_head`, `parse_request_line`, `parse_headers`, `content_length`, `write_response`. Each server keeps its own limits, timeouts and status codes.
- `client` (feature, empty for now): the shared reqwest facade lands in a later slice.

Depends on `oxa-proto` only.
