//! E7: JSON-RPC and the MCP client core (initialize, tools, resources, progress). Owner: task E7. Signatures below are frozen; bodies are yours.
// ported from pi packages/mcp/src/{client.ts,protocol/*}
use crate::{CallToolResult, McpResource, McpTool, ToolAnnotations, Transport, PROTOCOL_VERSION};
use oxa_proto::{codes, image_placeholder, CancelToken, OxaError, Value};
use serde_json::{json, Map};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A `notifications/progress` callback: (progress, total, message).
type ProgressFn<'a> = dyn FnMut(f64, Option<f64>, Option<&str>) + 'a;

/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
/// Longest single wait inside a request, so cancel and timeout are polled.
const POLL: Duration = Duration::from_millis(50);

fn mcp_err(msg: impl Into<String>) -> OxaError {
    OxaError::new(codes::MCP, msg)
}

/// Build a JSON-RPC 2.0 request `{"jsonrpc":"2.0","id":id,"method":method,"params":params}`
/// (`params` omitted when `Null`).
pub fn rpc_request(id: u64, method: &str, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("jsonrpc".into(), json!("2.0"));
    m.insert("id".into(), json!(id));
    m.insert("method".into(), json!(method));
    if !params.is_null() {
        m.insert("params".into(), params.clone());
    }
    Value::Object(m)
}

/// Build a JSON-RPC notification (no `id`).
pub fn rpc_notification(method: &str, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("jsonrpc".into(), json!("2.0"));
    m.insert("method".into(), json!(method));
    if !params.is_null() {
        m.insert("params".into(), params.clone());
    }
    Value::Object(m)
}

/// A decoded incoming message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A response to request `id`: `Ok(result)` or `Err((code, message))`.
    Response {
        id: u64,
        result: Result<Value, (i64, String)>,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// A server-to-client request (`ping`, `roots/list`, ..).
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// Classify an incoming JSON value. `Err(mcp)` for anything that is not JSON-RPC 2.0.
pub fn rpc_decode(msg: &Value) -> Result<Incoming, OxaError> {
    let bad = |why: &str| mcp_err(format!("malformed MCP message: {why}"));
    let obj = msg.as_object().ok_or_else(|| bad("not a JSON object"))?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(bad("jsonrpc is not \"2.0\""));
    }
    if let Some(method) = obj.get("method") {
        let method = method
            .as_str()
            .ok_or_else(|| bad("method is not a string"))?
            .to_string();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        return match obj.get("id") {
            None => Ok(Incoming::Notification { method, params }),
            Some(id) if id.is_string() || id.is_number() => Ok(Incoming::Request {
                id: id.clone(),
                method,
                params,
            }),
            Some(_) => Err(bad("request id is neither a string nor a number")),
        };
    }
    let id = obj
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("response without a numeric id"))?;
    if let Some(err) = obj.get("error").filter(|e| !e.is_null()) {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Ok(Incoming::Response {
            id,
            result: Err((code, message)),
        });
    }
    match obj.get("result") {
        Some(r) => Ok(Incoming::Response {
            id,
            result: Ok(r.clone()),
        }),
        None => Err(bad("response has neither result nor error")),
    }
}

/// The text a tool result contributes to the wire: the `text` of every text block in order,
/// joined with `\n`; an image block becomes `oxa_proto::image_placeholder` with the name
/// `image`; a `resource_link` becomes one line naming `read_mcp_resource`, the server and the
/// URI; an embedded text resource contributes its text; without any block, the JSON text of
/// `structured_content`, else the empty string. Deterministic (W4).
pub fn result_text(server: &str, r: &CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in &r.content {
        let text_of = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => parts.push(text_of("text")),
            // MCP image blocks carry no pixel size; the placeholder reports 0x0.
            "image" => parts.push(image_placeholder("image", 0, 0)),
            "resource_link" => {
                let uri = text_of("uri");
                parts.push(format!(
                    "[resource link: read_mcp_resource(server: \"{server}\", uri: \"{uri}\")]"
                ));
            }
            "resource" => {
                let res = block.get("resource").unwrap_or(&Value::Null);
                match res.get("text").and_then(Value::as_str) {
                    Some(t) => parts.push(t.to_string()),
                    None => {
                        let uri = res.get("uri").and_then(Value::as_str).unwrap_or("");
                        parts.push(format!("[binary resource {uri} omitted]"));
                    }
                }
            }
            _ => {}
        }
    }
    if r.content.is_empty() {
        return match &r.structured_content {
            Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
            None => String::new(),
        };
    }
    parts.join("\n")
}

/// Receives `notifications/message` (server logging): `(level, logger, message)`.
pub type LogSink = Box<dyn FnMut(&str, &str, &str) + Send>;

/// One initialized MCP session over a transport. Calls are serialised: one request at a time
/// per client; callers on other threads wait.
pub struct McpClient {
    pub(crate) transport: Box<dyn Transport>,
    pub(crate) next_id: u64,
    pub(crate) timeout: Duration,
    pub(crate) instructions: Option<String>,
    pub(crate) capabilities: Value,
    pub(crate) log: Option<LogSink>,
    pub(crate) closed: bool,
}

/// `cursor` as the params object of a paged list request.
fn cursor_params(cursor: Option<&str>) -> Value {
    match cursor {
        Some(c) => json!({ "cursor": c }),
        None => json!({}),
    }
}

/// The `nextCursor` of a paged result, `None` when absent or empty.
fn next_cursor(result: &Value) -> Option<String> {
    result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// MCP Apps resources (`ui://` or the `mcp-app` HTML profile) are not offered to the model.
fn is_app_resource(v: &Value) -> bool {
    let uri = v
        .get("uri")
        .or_else(|| v.get("uriTemplate"))
        .and_then(Value::as_str)
        .unwrap_or("");
    uri.starts_with("ui://") || v.get("mimeType").and_then(Value::as_str) == Some("text/html;profile=mcp-app")
}

/// A listed resource or template: `uri` (or `uriTemplate`) and `name` are fields of their own,
/// the remaining fields are kept in `extra`.
fn parse_resource(v: &Value, uri_key: &str) -> Result<McpResource, OxaError> {
    let mut obj = v
        .as_object()
        .cloned()
        .ok_or_else(|| mcp_err("resource entry is not an object"))?;
    let uri = obj
        .remove(uri_key)
        .and_then(|u| u.as_str().map(str::to_string))
        .ok_or_else(|| mcp_err(format!("resource entry has no {uri_key}")))?;
    let name = obj
        .remove("name")
        .and_then(|n| n.as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(McpResource {
        uri,
        name,
        extra: Value::Object(obj),
    })
}

fn parse_tool(v: &Value) -> Result<McpTool, OxaError> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| mcp_err("tools/list: tool entry has no name"))?
        .to_string();
    let flag = |k: &str| {
        v.get("annotations")
            .and_then(|a| a.get(k))
            .and_then(Value::as_bool)
    };
    let annotations = ToolAnnotations {
        read_only_hint: flag("readOnlyHint"),
        destructive_hint: flag("destructiveHint"),
        idempotent_hint: flag("idempotentHint"),
        open_world_hint: flag("openWorldHint"),
    };
    Ok(McpTool {
        name,
        description: v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        input_schema: v
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" })),
        annotations,
    })
}

impl McpClient {
    /// Run the `initialize` handshake (client name `oxa`, [`crate::PROTOCOL_VERSION`]) and send
    /// `notifications/initialized`. Errors: `timeout`, `mcp`, or the transport's error
    /// (including [`crate::CODE_NEEDS_AUTH`]).
    pub fn connect(
        transport: Box<dyn Transport>,
        client_version: &str,
        timeout: Duration,
    ) -> Result<McpClient, OxaError> {
        let mut c = McpClient {
            transport,
            next_id: 1,
            timeout,
            instructions: None,
            capabilities: json!({}),
            log: None,
            closed: false,
        };
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "oxa", "version": client_version },
        });
        let result = match c.request("initialize", &params, None, None) {
            Ok(r) => r,
            Err(e) => {
                c.close();
                return Err(e);
            }
        };
        let negotiated = result.get("protocolVersion").and_then(Value::as_str);
        if negotiated != Some(PROTOCOL_VERSION) {
            c.close();
            return Err(mcp_err(format!(
                "server speaks MCP protocol {} and oxa speaks {PROTOCOL_VERSION}",
                negotiated.unwrap_or("(none)")
            )));
        }
        c.capabilities = result
            .get("capabilities")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        c.instructions = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_string);
        c.notify("notifications/initialized", &Value::Null)?;
        Ok(c)
    }

    pub fn set_log_sink(&mut self, sink: LogSink) {
        self.log = Some(sink);
    }

    /// The server's `instructions` from `initialize`.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// `true` when the server declared the `resources` capability.
    pub fn has_resources(&self) -> bool {
        self.capabilities.get("resources").is_some_and(|v| !v.is_null())
    }

    /// `tools/list`, all pages, in server order.
    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, OxaError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            let r = self.request("tools/list", &cursor_params(cursor.as_deref()), None, None)?;
            let tools = r
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| mcp_err("tools/list: result has no tools array"))?;
            for t in tools {
                out.push(parse_tool(t)?);
            }
            cursor = next_cursor(&r);
            match &cursor {
                None => return Ok(out),
                Some(c) => {
                    if !seen.insert(c.clone()) {
                        return Err(mcp_err("tools/list: the server repeats a cursor"));
                    }
                }
            }
        }
    }

    /// `tools/call`. The per-request timeout restarts on every `notifications/progress` for
    /// this call (`on_progress(progress, total, message)`). `cancel` sends
    /// `notifications/cancelled` and returns `Err(aborted)`. Never retried. A JSON-RPC error is
    /// `Err(mcp)`; a result with `isError` is `Ok` with `is_error = true`.
    pub fn call_tool(
        &mut self,
        name: &str,
        args: &Value,
        cancel: &CancelToken,
        on_progress: &mut ProgressFn<'_>,
    ) -> Result<CallToolResult, OxaError> {
        let args = if args.is_null() { json!({}) } else { args.clone() };
        let params = json!({ "name": name, "arguments": args });
        let r = self.request("tools/call", &params, Some(cancel), Some(on_progress))?;
        Ok(CallToolResult {
            content: r
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            structured_content: r.get("structuredContent").filter(|v| !v.is_null()).cloned(),
            is_error: r.get("isError").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// `resources/list`, one page. Returns the resources and the next cursor. Resources for
    /// MCP Apps (`ui://`, `text/html;profile=mcp-app`) are omitted.
    pub fn list_resources(
        &mut self,
        cursor: Option<&str>,
    ) -> Result<(Vec<McpResource>, Option<String>), OxaError> {
        let r = self.request("resources/list", &cursor_params(cursor), None, None)?;
        let list = r
            .get("resources")
            .and_then(Value::as_array)
            .ok_or_else(|| mcp_err("resources/list: result has no resources array"))?;
        let mut out = Vec::new();
        for v in list.iter().filter(|v| !is_app_resource(v)) {
            out.push(parse_resource(v, "uri")?);
        }
        Ok((out, next_cursor(&r)))
    }

    /// `resources/templates/list`, one page.
    pub fn list_resource_templates(
        &mut self,
        cursor: Option<&str>,
    ) -> Result<(Vec<McpResource>, Option<String>), OxaError> {
        let r = self.request("resources/templates/list", &cursor_params(cursor), None, None)?;
        let list = r
            .get("resourceTemplates")
            .and_then(Value::as_array)
            .ok_or_else(|| mcp_err("resources/templates/list: result has no resourceTemplates array"))?;
        let mut out = Vec::new();
        for v in list.iter().filter(|v| !is_app_resource(v)) {
            out.push(parse_resource(v, "uriTemplate")?);
        }
        Ok((out, next_cursor(&r)))
    }

    /// `resources/read`: the `contents` array as received.
    pub fn read_resource(&mut self, uri: &str) -> Result<Vec<Value>, OxaError> {
        let r = self.request("resources/read", &json!({ "uri": uri }), None, None)?;
        r.get("contents")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| mcp_err("resources/read: result has no contents array"))
    }

    /// Answer server requests (`ping` -> `{}`, anything else -> method-not-found) and dispatch
    /// notifications that arrived while idle. Called by the manager between calls.
    pub fn pump(&mut self) -> Result<(), OxaError> {
        self.ensure_open()?;
        while let Some(msg) = self.transport.recv(Duration::ZERO)? {
            match rpc_decode(&msg)? {
                Incoming::Request { id, method, .. } => self.answer(id, &method)?,
                Incoming::Notification { method, params } => self.dispatch(&method, &params),
                // A response nobody waits for (a late answer to a timed-out call).
                Incoming::Response { .. } => {}
            }
        }
        Ok(())
    }

    pub fn stderr_tail(&self) -> String {
        self.transport.stderr_tail()
    }

    /// Close the transport. Idempotent.
    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.transport.close();
        }
    }

    fn ensure_open(&self) -> Result<(), OxaError> {
        if self.closed {
            Err(mcp_err("MCP connection is closed"))
        } else {
            Ok(())
        }
    }

    fn notify(&mut self, method: &str, params: &Value) -> Result<(), OxaError> {
        self.ensure_open()?;
        self.transport.send(&rpc_notification(method, params))
    }

    /// Reply to a server request.
    fn answer(&mut self, id: Value, method: &str) -> Result<(), OxaError> {
        let reply = if method == "ping" {
            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": METHOD_NOT_FOUND, "message": format!("method not found: {method}") },
            })
        };
        self.transport.send(&reply)
    }

    /// Notifications that need no reply: server logging goes to the sink; the rest is ignored.
    fn dispatch(&mut self, method: &str, params: &Value) {
        if method != "notifications/message" {
            return;
        }
        let Some(sink) = self.log.as_mut() else {
            return;
        };
        let level = params.get("level").and_then(Value::as_str).unwrap_or("info");
        let logger = params.get("logger").and_then(Value::as_str).unwrap_or("");
        let text = match params.get("data") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        };
        sink(level, logger, &text);
    }

    /// One request and its response. Progress for this request (token = request id) resets the
    /// timeout; a cancelled or timed-out request sends `notifications/cancelled`.
    fn request(
        &mut self,
        method: &str,
        params: &Value,
        cancel: Option<&CancelToken>,
        mut progress: Option<&mut ProgressFn<'_>>,
    ) -> Result<Value, OxaError> {
        self.ensure_open()?;
        let id = self.next_id;
        self.next_id += 1;
        let mut params = params.clone();
        if progress.is_some() {
            if params.is_null() {
                params = json!({});
            }
            if let Some(obj) = params.as_object_mut() {
                obj.insert("_meta".into(), json!({ "progressToken": id }));
            }
        }
        self.transport.send(&rpc_request(id, method, &params))?;
        let mut deadline = Instant::now() + self.timeout;
        loop {
            if cancel.is_some_and(|c| c.is_cancelled()) {
                let _ = self.notify(
                    "notifications/cancelled",
                    &json!({ "requestId": id, "reason": "cancelled by oxa" }),
                );
                return Err(OxaError::new(
                    codes::ABORTED,
                    format!("MCP {method} was cancelled"),
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                let _ = self.notify(
                    "notifications/cancelled",
                    &json!({ "requestId": id, "reason": "timed out" }),
                );
                return Err(OxaError::new(
                    codes::TIMEOUT,
                    format!("MCP {method} timed out after {:.1} s", self.timeout.as_secs_f64()),
                ));
            }
            let Some(msg) = self.transport.recv((deadline - now).min(POLL))? else {
                continue;
            };
            match rpc_decode(&msg)? {
                Incoming::Response { id: rid, result } if rid == id => {
                    return result
                        .map_err(|(code, message)| mcp_err(format!("{method}: {message} (code {code})")));
                }
                // A late answer to an earlier, timed-out request.
                Incoming::Response { .. } => {}
                Incoming::Notification { method: m, params: p } => {
                    if m == "notifications/progress" {
                        if p.get("progressToken").and_then(Value::as_u64) == Some(id) {
                            if let (Some(progress), Some(cb)) =
                                (p.get("progress").and_then(Value::as_f64), progress.as_deref_mut())
                            {
                                let total = p.get("total").and_then(Value::as_f64);
                                let message = p.get("message").and_then(Value::as_str);
                                cb(progress, total, message);
                            }
                            deadline = Instant::now() + self.timeout;
                        }
                    } else {
                        self.dispatch(&m, &p);
                    }
                }
                Incoming::Request {
                    id: sid, method: m, ..
                } => self.answer(sid, &m)?,
            }
        }
    }
}
