//! W0-FROZEN (architect-owned).
//!
//! # oxa-engine
//! The HTTP + SSE client for the oxe engine (plain `/v1/chat/completions`, plan sec. 3) and the
//! pure request builder.
//!
//! ## Contracts
//! * [`build_body`] and [`build_headers`] are pure. W1 (messages byte-append), W2 (exact echo),
//!   W3 (prefix bytes) and W6 (explicit sampling) are tested on their output; nothing else in
//!   the workspace builds a request.
//! * No HTTP library: HTTP/1.1 over `std::net::TcpStream`, plain HTTP (local or LAN engine).
//! * **Liveness.** SSE comments (`: prefill`, `: queued`) count as liveness. There is one silence
//!   limit (no byte for `silence_limit_ms`) and no wall-clock limit: 16 minutes of comments is
//!   not an error.
//! * **Retry.** Only before the first data byte, with the same `Idempotency-Key`. After the
//!   first data byte a broken stream is `stream_broken` and the partial reply is never pushed.
//! * Hints H1-H6 are optional for the server: a missing hint changes speed, never bytes.
#![forbid(unsafe_code)]

mod chat;
mod embedded;
mod handshake;
mod hints;
mod http;
mod info;
mod sse;

pub use chat::{build_body, build_body_for, HttpEngine, ReplyAssembler};
pub use embedded::{EmbeddedEngine, EmbeddedGateway, GatewayContext, GatewayDiscovery};
pub use handshake::{handshake_warning, NEEDED_CAPABILITIES, PROTOCOL_UNDERSTOOD};
pub use hints::{build_headers, new_idem_key};
pub use http::{http_request, HttpRequest, HttpResponse, Url};
pub use info::{count_tokens, fetch_info, fetch_info_for, parse_info, parse_info_for, DEFAULT_MAX_OUTPUT};
pub use sse::{post_sse, Flow, SseEnd, SseEvent, SseParser, StreamOpts};

pub use oxa_proto::{ChatRequest, Engine, EngineInfo, Hints};
