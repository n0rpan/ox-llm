//! Private synthetic protocol driver. No network or credential is printed.
use oxc_gateway::{openrouter, secrets};
use serde_json::{json, Value};
use std::{
    io::{self, Read},
    path::Path,
};
fn main() {
    let mut text = String::new();
    io::stdin().take(524289).read_to_string(&mut text).unwrap();
    assert!(text.len() <= 524288);
    let spec: Value = serde_json::from_str(&text).unwrap();
    let key = spec["key_path"]
        .as_str()
        .map(|path| secrets::read(Path::new(path), "openrouter").unwrap());
    let result = match spec["op"].as_str().unwrap() {
        "request" => match openrouter::request(&spec["payload"], "synthetic/upstream", 512) {
            Ok(body) => json!({"body":body}),
            Err(error) => {
                json!({"status":error.status,"message":error.message,"code":error.code,"param":error.param})
            }
        },
        "response" => openrouter::response(spec["value"].clone(), "synthetic-cloud", key.as_ref()),
        "stream" => {
            let mut stream = openrouter::Stream::new("synthetic-cloud", key.as_ref());
            let mut bytes = Vec::new();
            for chunk in spec["chunks"].as_array().unwrap() {
                let chunk = chunk
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap() as u8)
                    .collect::<Vec<_>>();
                bytes.extend(stream.feed(&chunk));
            }
            bytes.extend(stream.finish());
            json!({"wire": String::from_utf8(bytes).unwrap()})
        }
        _ => panic!("unknown synthetic operation"),
    };
    println!("{result}");
}
