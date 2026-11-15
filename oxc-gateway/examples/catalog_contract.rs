//! Test driver only: JSON over stdin/stdout, no listener or installation path.
use oxc_gateway::catalog::Catalog;
use serde_json::{json, Value};
use std::io::{self, Read};
fn main() {
    let mut input = String::new();
    io::stdin()
        .take(65537)
        .read_to_string(&mut input)
        .expect("contract input");
    assert!(input.len() <= 65536, "contract input bound");
    let input: Value = serde_json::from_str(&input).expect("contract JSON");
    let catalog = Catalog::from_config(&input["config"], input["offline"] == true);
    let output = catalog.map(|catalog| {
        json!({"models":catalog.models(),"chooser":catalog.chooser(),
        "status":catalog.status_fragment(),"health":catalog.health(),
        "model":catalog.model("synthetic-cloud"),"unknown":catalog.model("synthetic-unknown"),
        "startup":catalog.startup_line()})
    });
    println!(
        "{}",
        serde_json::to_string(&output).expect("contract output")
    );
}
