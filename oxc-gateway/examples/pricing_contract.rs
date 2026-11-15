//! Synthetic pricing contract driver, no I/O beyond stdin/stdout.
use oxc_gateway::pricing::{Cache, Price};
use serde_json::{json, Value};
use std::io::{self, Read};
fn main() {
    let mut text = String::new();
    io::stdin()
        .take(65537)
        .read_to_string(&mut text)
        .expect("contract input");
    assert!(text.len() <= 65536);
    let input: Value = serde_json::from_str(&text).expect("contract JSON");
    let Some(price) = Price::from_model(&input["model"]) else {
        println!(
            "{}",
            json!({"pricing":null,"actual":null,"reservation":null})
        );
        return;
    };
    let provider = input["provider"].as_str().unwrap();
    let upstream = input["upstream_model"].as_str().unwrap();
    let cache = match (input["read"].as_u64(), input["write"].as_u64()) {
        (Some(read), Some(write)) => Cache::Tokens { read, write },
        _ => Cache::Invalid,
    };
    let cost = price.cost(
        provider,
        upstream,
        input["prompt"].as_u64().unwrap_or(0),
        input["output"].as_u64().unwrap_or(0),
        cache,
    );
    let bound = price.reservation(
        provider,
        upstream,
        input["context"].as_u64().unwrap(),
        input["output_limit"].as_u64().unwrap(),
    );
    println!(
        "{}",
        json!({"pricing":price.normalized(),"actual":cost,"reservation":bound})
    );
}
