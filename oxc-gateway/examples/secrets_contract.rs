//! Test driver: synthetic paths/keys only. Never prints a credential or path.
use oxc_gateway::{redaction, secrets};
use serde_json::{json, Value};
use std::{
    io::{self, Read},
    path::Path,
};
fn main() {
    let mut text = String::new();
    io::stdin()
        .take(131073)
        .read_to_string(&mut text)
        .expect("contract input");
    assert!(text.len() <= 131072);
    let input: Value = serde_json::from_str(&text).expect("contract JSON");
    let path = Path::new(input["path"].as_str().expect("synthetic path"));
    let result = match secrets::read(path, "openrouter") {
        Err(error) => json!({"error":error.to_string()}),
        Ok(key) => {
            assert!(!format!("{key:?}").contains(key.expose()), "redacted Debug");
            if let Some(runs) = input["runs"].as_array() {
                let outputs = runs
                    .iter()
                    .map(|run| {
                        let mut stream = redaction::Stream::new(&key);
                        let mut chunks = run
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|part| {
                                String::from_utf8(stream.feed(part.as_str().unwrap().as_bytes()))
                                    .unwrap()
                            })
                            .collect::<Vec<_>>();
                        chunks.push(String::from_utf8(stream.finish()).unwrap());
                        chunks
                    })
                    .collect::<Vec<_>>();
                json!({"outputs":outputs})
            } else if input.get("json").is_some() {
                let mut value = input["json"].clone();
                redaction::value(&mut value, &key);
                json!({"redacted":value})
            } else if let Some(replace) = input["replace"].as_str() {
                std::fs::rename(replace, path).expect("owned fixture rotation");
                match secrets::read(path, "openrouter") {
                    Ok(second) => {
                        json!({"first_ok":key.expose()==input["expected"].as_str().unwrap(),"second_ok":second.expose()==input["second"].as_str().unwrap()})
                    }
                    Err(error) => json!({"error":error.to_string()}),
                }
            } else {
                json!({"ok":key.expose()==input["expected"].as_str().unwrap()})
            }
        }
    };
    println!(
        "{}",
        serde_json::to_string(&result).expect("safe contract result")
    );
}
