//! Explicit settings for tests of the HTTP engine contract.
//! Native-selection tests deliberately do not use this fixture.
use oxa_fake::TestEnv;
use serde_json::{json, Value};

pub fn envs(env: &TestEnv, url: &str) -> Vec<(String, String)> {
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("valid private HTTP fixture settings"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => panic!("read private HTTP fixture settings: {error}"),
    };
    // Repeated launches must not rewrite an already configured fixture's settings.
    if settings
        .pointer("/oxa/engine/inProcessCloud")
        .and_then(Value::as_bool)
        == Some(false)
        && settings
            .pointer("/oxa/engine/inProcessLocal")
            .and_then(Value::as_bool)
            == Some(false)
    {
        return env.envs(url);
    }
    settings["oxa"]["engine"]["inProcessCloud"] = json!(false);
    settings["oxa"]["engine"]["inProcessLocal"] = json!(false);
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    env.envs(url)
}
