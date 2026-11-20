use super::listener::{Failure, Request};
use oxa_http::server::{content_length, parse_headers, parse_request_line, split_head, Headers, RequestLine};
use serde_json::Value;

pub(super) struct Head {
    pub chat: bool,
    pub bearer: String,
    pub length: usize,
}
/// The single `Content-Length`, canonical decimal. An all-digit value too big for `usize` is a
/// size problem (413); every other defect is a malformed request (400).
fn length_of(headers: &Headers<'_>, value: &str) -> Result<usize, Failure> {
    match content_length(headers) {
        Ok(n) => Ok(n.unwrap_or(0)),
        Err(_)
            if headers.count("content-length") == 1
                && !value.is_empty()
                && value.bytes().all(|b| b.is_ascii_digit())
                && value.parse::<usize>().is_err() =>
        {
            Err(Failure::TooLarge)
        }
        Err(_) => Err(Failure::BadRequest),
    }
}

pub(super) fn head(bytes: &[u8], max_body: usize) -> Result<Head, Failure> {
    let (line, block) = split_head(bytes).map_err(|_| Failure::BadRequest)?;
    // Only this exact request line is served; any other shape is an unknown route.
    let chat = match parse_request_line(line) {
        Ok(RequestLine {
            method: "GET",
            target: "/v1/models",
            version: "HTTP/1.1",
        }) => false,
        Ok(RequestLine {
            method: "POST",
            target: "/v1/chat/completions",
            version: "HTTP/1.1",
        }) => true,
        _ => return Err(Failure::NotFound),
    };
    // A 65th header line is refused as too large, but only once the 64 before it were well formed.
    let block = match block.match_indices("\r\n").nth(63) {
        Some((end, _)) => {
            parse_headers(&block[..end]).map_err(|_| Failure::BadRequest)?;
            return Err(Failure::TooLarge);
        }
        None => block,
    };
    let headers = parse_headers(block).map_err(|_| Failure::BadRequest)?;
    let (mut bearer, mut length, mut host, mut content_type) = (None, None, false, false);
    for (name, value) in headers.iter() {
        match name.to_ascii_lowercase().as_str() {
            "host" if !host && !value.is_empty() => host = true,
            "host" | "transfer-encoding" | "expect" => return Err(Failure::BadRequest),
            "authorization" if bearer.is_none() => {
                let value = value.strip_prefix("Bearer ").ok_or(Failure::Unauthorized)?;
                if value.len() != 64
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(Failure::Unauthorized);
                }
                bearer = Some(value.to_owned());
            }
            "authorization" => return Err(Failure::BadRequest),
            "content-length" if length.is_none() => {
                length = Some(length_of(&headers, value)?);
            }
            "content-length" => return Err(Failure::BadRequest),
            "content-type" if !content_type => {
                if value.split(';').next() != Some("application/json") {
                    return Err(Failure::BadRequest);
                }
                content_type = true;
            }
            "content-type" => return Err(Failure::BadRequest),
            _ => (), // Forwarded headers deliberately confer no peer identity.
        }
    }
    let length = length.unwrap_or(0);
    if length > max_body {
        return Err(Failure::TooLarge);
    }
    if !host || (chat && (!content_type || length == 0)) || (!chat && length != 0) {
        return Err(Failure::BadRequest);
    }
    Ok(Head {
        chat,
        length,
        bearer: bearer.ok_or(Failure::Unauthorized)?,
    })
}

pub(super) fn request(bytes: &[u8], select: impl Fn(&str) -> Option<u32>) -> Result<Request, Failure> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| Failure::BadRequest)?;
    let map = v.as_object().ok_or(Failure::BadRequest)?;
    if map.keys().any(|key| {
        !matches!(
            key.as_str(),
            "model"
                | "messages"
                | "stream"
                | "max_tokens"
                | "max_completion_tokens"
                | "temperature"
                | "top_p"
                | "stream_options"
        )
    }) || v["stream"] != true
    {
        return Err(Failure::BadRequest);
    }
    let model = v["model"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or(Failure::BadRequest)?;
    let max_output = select(model).ok_or(Failure::Forbidden)?;
    let messages = v["messages"]
        .as_array()
        .filter(|m| !m.is_empty() && m.len() <= 512)
        .ok_or(Failure::BadRequest)?;
    for message in messages {
        let m = message.as_object().ok_or(Failure::BadRequest)?;
        if m.len() != 2
            || !matches!(message["role"].as_str(), Some("system" | "user" | "assistant"))
            || !message["content"].is_string()
        {
            return Err(Failure::BadRequest);
        }
    }
    if map.contains_key("max_tokens") && map.contains_key("max_completion_tokens") {
        return Err(Failure::BadRequest);
    }
    let cap = map
        .get("max_tokens")
        .or_else(|| map.get("max_completion_tokens"))
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0 && *n <= max_output)
                .ok_or(Failure::BadRequest)
        })
        .transpose()?
        .unwrap_or(max_output);
    for (key, maximum) in [("temperature", 2.0), ("top_p", 1.0)] {
        if let Some(v) = map.get(key) {
            let n = v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= maximum)
                .ok_or(Failure::BadRequest)?;
            let _ = n;
        }
    }
    let include_usage = match map.get("stream_options") {
        None => false,
        Some(options) => {
            if options.as_object().ok_or(Failure::BadRequest)?.len() != 1 {
                return Err(Failure::BadRequest);
            }
            options["include_usage"].as_bool().ok_or(Failure::BadRequest)?
        }
    };
    Ok(Request {
        model: model.into(),
        messages: messages.clone(),
        max_output: cap,
        temperature: map.get("temperature").and_then(Value::as_f64),
        top_p: map.get("top_p").and_then(Value::as_f64),
        include_usage,
    })
}
