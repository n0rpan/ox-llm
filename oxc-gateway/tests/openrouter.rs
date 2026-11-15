#![cfg(feature = "openrouter")]
use oxc_gateway::{openrouter::Stream, secrets};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "oxc-router-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn key(&self) -> secrets::Key {
        let path = self.0.join("provider.json");
        fs::write(
            &path,
            br#"{"openrouter":{"api_key":"synthetic-router-key"}}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        secrets::read(&path, "openrouter").unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn frame(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}
fn events(wire: &[u8]) -> Vec<Value> {
    String::from_utf8(wire.into())
        .unwrap()
        .split("\n\n")
        .filter_map(|event| {
            let data = event.strip_prefix("data: ")?;
            if data == "[DONE]" || data.is_empty() {
                None
            } else {
                Some(serde_json::from_str(data).unwrap())
            }
        })
        .collect()
}
#[test]
fn tool_names_are_redacted_in_place_and_emitted_whole_before_any_finish() {
    let home = Home::new();
    let key = home.key();
    for name in ["lookups", key.expose()] {
        let mut stream = Stream::new("synthetic-cloud", Some(&key));
        let out = stream.feed(&frame(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"function":{"name":name,"arguments":"{}"}}]},"finish_reason":null}]})));
        let values = events(&out);
        assert_eq!(values.len(), 1);
        assert_eq!(
            values[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            if name == "lookups" {
                "lookups"
            } else {
                "[redacted]"
            }
        );
        let end = stream.feed(b"data: [DONE]\n\n");
        assert!(
            events(&end).is_empty(),
            "no later name fragment may be dispatched"
        );
    }
}
#[test]
fn split_secret_arguments_and_parallel_choices_are_independent_of_network_chunks() {
    let home = Home::new();
    let key = home.key();
    let mut input = Vec::new();
    for value in [
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,
        "function":{"name":"lookup","arguments":"synthetic-"}}]}}]}),
        json!({"choices":[{"index":1,"delta":{"content":"other"},"finish_reason":"stop"}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,
        "function":{"arguments":"router-key"}}]},"finish_reason":"tool_calls"}]}),
    ] {
        input.extend(frame(value));
    }
    input.extend_from_slice(b"data: [DONE]\n\n");
    let run = |chunks: Vec<&[u8]>| {
        let mut stream = Stream::new("synthetic-cloud", Some(&key));
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(stream.feed(chunk));
        }
        out.extend(stream.finish());
        out
    };
    let expected = run(vec![&input]);
    for split in 0..input.len() {
        assert_eq!(run(vec![&input[..split], &input[split..]]), expected);
    }
    assert_eq!(run(input.chunks(1).collect()), expected);
    let mut arguments = String::new();
    for value in events(&expected) {
        if let Some(text) =
            value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
        {
            arguments.push_str(text);
        }
    }
    assert_eq!(arguments, "[redacted]");
}
#[test]
fn held_content_precedes_the_single_terminal_usage_notification() {
    let home = Home::new();
    let key = home.key();
    let mut stream = Stream::new("synthetic-cloud", Some(&key));
    let mut out = stream.feed(&frame(
        json!({"choices":[{"index":0,"delta":{"content":"syn"},
        "finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
    ));
    out.extend(stream.feed(b"data: [DONE]\n\n"));
    out.extend(stream.finish());
    let values = events(&out);
    assert_eq!(values.len(), 3);
    assert_eq!(values[0]["choices"][0]["finish_reason"], Value::Null);
    assert_eq!(values[1]["choices"][0]["delta"]["content"], "syn");
    assert_eq!(values[2]["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        values
            .iter()
            .filter(|value| value.get("usage").is_some())
            .count(),
        1
    );
}
#[test]
fn eof_flushes_content_and_arguments_without_done_or_a_finish_reason() {
    let home = Home::new();
    let key = home.key();
    let mut stream = Stream::new("synthetic-cloud", Some(&key));
    let mut out = stream.feed(&frame(
        json!({"choices":[{"index":0,"delta":{"content":"syn",
        "tool_calls":[{"index":0,"function":{"name":"lookup","arguments":"syn"}}]}}]}),
    ));
    out.extend(stream.finish());
    let values = events(&out);
    assert_eq!(
        values
            .iter()
            .filter_map(|v| v["choices"][0]["delta"]["content"].as_str())
            .collect::<String>(),
        "syn"
    );
    assert_eq!(
        values
            .iter()
            .filter_map(
                |v| v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            )
            .collect::<String>(),
        "syn"
    );
    assert!(stream.finish().is_empty());
}
#[test]
fn escaped_credentials_in_scalar_frames_and_tail_metadata_are_redacted() {
    let home = Home::new();
    let secret = "synthetic\"router\\key";
    let path = home.0.join("escaped.json");
    fs::write(&path, json!({"openrouter":{"api_key":secret}}).to_string()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let key = secrets::read(&path, "openrouter").unwrap();
    let mut stream = Stream::new("synthetic-cloud", Some(&key));
    let out = stream.feed(&frame(json!(secret)));
    assert_eq!(events(&out), vec![json!("[redacted]")]);
    let mut out = stream.feed(&frame(
        json!({"choices":[{"index":secret,"delta":{"content":"syn"},"finish_reason":"stop"}]}),
    ));
    out.extend(stream.feed(b"data: [DONE]\n\n"));
    out.extend(stream.finish());
    for value in events(&out) {
        assert_eq!(value["choices"][0]["index"], "[redacted]");
    }
}
#[test]
fn changing_a_tool_name_mid_stream_cannot_reassemble_a_credential() {
    let home = Home::new();
    let key = home.key();
    let mut stream = Stream::new("synthetic-cloud", Some(&key));
    let mut out = Vec::new();
    for name in ["synthetic-", "router-key"] {
        out.extend(
            stream.feed(&frame(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"name":name,"arguments":""}}]}}]}))),
        );
    }
    out.extend(stream.finish());
    let values = events(&out);
    let names = values
        .iter()
        .filter_map(|v| v["choices"][0]["delta"]["tool_calls"][0]["function"]["name"].as_str())
        .collect::<String>();
    assert!(
        !names.contains(key.expose()),
        "fragmented name exposed the synthetic credential"
    );
    assert_eq!(
        values.last().unwrap()["error"]["code"],
        "cloud_invalid_response"
    );
    assert!(stream.is_failed());
}
