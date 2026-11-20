//! Pure subscription-backend constraints on an already translated Responses request.
use serde_json::Value;

/// Adapt a translated Responses object, matching `cloud_chatgpt.py::adapt`.
///
/// Input validation belongs to the Responses translator. No transport, credentials,
/// or continuation state is read here; non-object values are returned unchanged.
pub fn adapt(mut body: Value) -> Value {
    let Some(fields) = body.as_object_mut() else {
        return body;
    };
    let input = match fields.get_mut("input").map(Value::take) {
        Some(Value::Array(input)) => input,
        _ => Vec::new(),
    };
    let mut inputs = Vec::new();
    let mut instructions = Vec::new();
    for part in input {
        if matches!(
            part.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        ) {
            let text = match &part["content"] {
                Value::String(text) => text.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter_map(|block| block.get("text")?.as_str())
                    .collect(),
                _ => String::new(),
            };
            if !text.is_empty() {
                instructions.push(text);
            }
        } else {
            inputs.push(part);
        }
    }
    let instructions = if instructions.is_empty() {
        "You are a helpful assistant.".to_owned()
    } else {
        instructions.join("\n\n")
    };
    fields.insert("input".into(), Value::Array(inputs));
    fields.insert("instructions".into(), Value::String(instructions));
    // Preserve the order of the remaining fields for a stable serialized prefix.
    fields.shift_remove("max_output_tokens");
    fields.insert("stream".into(), Value::Bool(true));
    fields.insert("store".into(), Value::Bool(false));
    body
}

/// Subscription protocol headers only; authorization belongs to the caller.
/// These mirror the fixed subscription headers in the legacy cloud transport.
pub fn headers(account_id: &str) -> [(&'static str, &str); 4] {
    [
        ("chatgpt-account-id", account_id),
        ("OpenAI-Beta", "responses=experimental"),
        ("Accept", "text/event-stream"),
        ("originator", "oxc"),
    ]
}

/// Headers of the plan's model list: the account and the client name, as the Codex CLI sends.
pub fn catalog_headers(account_id: &str) -> [(&'static str, &str); 2] {
    [("chatgpt-account-id", account_id), ("originator", "oxc")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn moves_instructions_and_preserves_remaining_input_and_fields() {
        let body = json!({
            "model": "synthetic-model",
            "input": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "Question"},
                {"role": "developer", "content": [
                    {"type": "input_text", "text": "Use "},
                    null,
                    {"type": "input_text", "text": "plain text."},
                    {"type": "input_image", "image_url": "synthetic-image"}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "found"},
                {"role": "assistant", "content": "Answer"}
            ],
            "instructions": "Replaced, not prepended.",
            "max_output_tokens": 100,
            "stream": false,
            "store": true,
            "tools": [{"type": "function", "name": "lookup"}],
            "reasoning": {"effort": "low"},
            "include": ["reasoning.encrypted_content"]
        });
        let mut expected = body.clone();
        expected["input"] = json!([
            body["input"][1],
            body["input"][3],
            body["input"][4],
            body["input"][5]
        ]);
        expected["instructions"] = json!("Be brief.\n\nUse plain text.");
        expected.as_object_mut().unwrap().remove("max_output_tokens");
        expected["stream"] = json!(true);
        expected["store"] = json!(false);
        assert_eq!(adapt(body), expected);
    }

    #[test]
    fn empty_instruction_content_uses_default() {
        for body in [
            json!({}),
            json!({"input": null}),
            json!({"input": []}),
            json!({"input": [
                {"role": "system"},
                {"role": "developer", "content": null},
                {"role": "system", "content": ""},
                {"role": "developer", "content": []}
            ], "instructions": "Replaced."}),
        ] {
            assert_eq!(
                adapt(body),
                json!({
                    "input": [], "instructions": "You are a helpful assistant.",
                    "stream": true, "store": false
                })
            );
        }
    }

    #[test]
    fn skips_empty_instructions_without_trimming_or_reordering() {
        assert_eq!(
            adapt(json!({"input": [
                {"role": "developer", "content": "First"},
                {"role": "system", "content": ""},
                {"role": "system", "content": " "},
                {"role": "developer", "content": "Last"},
                "unmodified input"
            ]})),
            json!({
                "input": ["unmodified input"],
                "instructions": "First\n\n \n\nLast", "stream": true, "store": false
            })
        );
    }

    #[test]
    fn stable_prefix_has_byte_stable_output() {
        let body: Value = serde_json::from_str(
            r#"{"model":"synthetic-model","input":[{"role":"system","content":"Frozen prefix."},{"role":"user","content":"Question"}],"max_output_tokens":100,"tools":[],"stream":false,"store":true}"#,
        ).unwrap();
        let first = serde_json::to_vec(&adapt(body.clone())).unwrap();
        let second = serde_json::to_vec(&adapt(body)).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, br#"{"model":"synthetic-model","input":[{"role":"user","content":"Question"}],"tools":[],"stream":true,"store":false,"instructions":"Frozen prefix."}"#);
    }

    #[test]
    fn fixed_headers_only_vary_by_account_id() {
        for account_id in ["synthetic-account", "another-account", ""] {
            assert_eq!(
                headers(account_id),
                [
                    ("chatgpt-account-id", account_id),
                    ("OpenAI-Beta", "responses=experimental"),
                    ("Accept", "text/event-stream"),
                    ("originator", "oxc"),
                ]
            );
        }
    }
}
