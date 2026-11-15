//! Synthetic pricing contract driver, no I/O beyond stdin/stdout.
use oxc_gateway::pricing::{Cache, Price};
use oxc_gateway::{
    accounting::{Budget, Model, Outcome, Usage},
    spend::append_private,
};
use serde_json::{json, Value};
use std::{
    io::{self, Read},
    path::PathBuf,
    time::Duration,
};
fn main() {
    let mut text = String::new();
    io::stdin()
        .take(65537)
        .read_to_string(&mut text)
        .expect("contract input");
    assert!(text.len() <= 65536);
    let input: Value = serde_json::from_str(&text).expect("contract JSON");
    let Some(price) = Price::from_model(&input["model"]) else {
        println!("{}", json!({"pricing":null,"actual":null,"reservation":null}));
        return;
    };
    let provider = input["provider"].as_str().unwrap();
    let upstream = input["upstream_model"].as_str().unwrap();
    if let Some(frames) = input["frames"].as_array() {
        let path = PathBuf::from(std::env::var("HOME").unwrap()).join("ledger/requests-synthetic.jsonl");
        let budget = Budget::new(path.clone());
        let model = Model {
            provider: provider.into(),
            upstream: upstream.into(),
            context: input["context"].as_u64().unwrap(),
            max_output: 512,
            price,
        };
        let mut hold = budget
            .reserve(&json!(1), true, &model, input["output_limit"].as_u64().unwrap())
            .unwrap();
        hold.mark_forwarded();
        let mut usage = Usage::default();
        for frame in frames {
            usage.observe(frame);
        }
        let outcome = match input["outcome"].as_str().unwrap() {
            "failed" => Outcome::Failed,
            "cancelled" => Outcome::Cancelled {
                request_bytes: input["request_bytes"].as_u64().unwrap() as usize,
                streamed_bytes: input["streamed_bytes"].as_u64().unwrap() as usize,
                usage_final: input["usage_final"].as_bool().unwrap(),
                elapsed: Duration::from_secs_f64(input["elapsed"].as_f64().unwrap()),
            },
            _ => Outcome::Complete,
        };
        let row = hold
            .finish(&usage, outcome, |row| append_private(&path, row))
            .unwrap();
        assert_eq!(row["cost_usd"].as_f64().unwrap(), budget.spend().unwrap());
        println!("{row}");
        return;
    }
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
