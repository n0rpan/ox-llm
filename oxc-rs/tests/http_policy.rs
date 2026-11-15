//! Source-derived HTTP gate decisions, exercised through the real transport.
//! Accepted requests terminate at an explicit test marker; this is not a daemon.
use axum::{extract::Request, http::Method, Router};
use oxc::{http_gate, transport};
use serde_json::{json, Value};
use std::{
    fs::{DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    process::Command,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const INPUTS: &str = include_str!("../../conformance/goldens/http-policy-inputs.json");
const GOLDENS: &str = include_str!("../../conformance/goldens/http-policy.json");
const CASE_ENV: &str = "OXC_CONFORMANCE_HTTP_POLICY_CASE";

#[test]
fn frozen_policy_contracts() {
    let inputs: Value = serde_json::from_str(INPUTS).expect("synthetic policy inputs");
    let expected: Value = serde_json::from_str(GOLDENS).expect("reference replies");
    assert_eq!(
        inputs.as_object().unwrap().len(),
        expected.as_object().unwrap().len()
    );
    if let Ok(name) = std::env::var(CASE_ENV) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(check(&name, &inputs[&name], expected[&name].clone()));
        return;
    }
    // Neither key-file nor environment tests may inherit a caller's credentials.
    // Each worker gets its own process, HOME and environment before Tokio starts.
    for (name, input) in inputs.as_object().unwrap() {
        let home =
            std::env::temp_dir().join(format!("oxc-policy-{}", http_gate::random_id().unwrap()));
        DirBuilder::new().mode(0o700).create(&home).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "frozen_policy_contracts", "--nocapture"])
            .current_dir(&home)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env(CASE_ENV, name);
        if let Some(environment) = input["env"].as_object() {
            for (key, value) in environment {
                command.env(key, value.as_str().unwrap());
            }
        }
        let result = command.output().unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        assert!(
            result.status.success(),
            "{name}: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

async fn check(name: &str, input: &Value, mut expected: Value) {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let mut config = input
        .get("api")
        .map_or_else(|| json!({}), |api| json!({"openai_api":api}));
    let key = home.join("synthetic-key");
    let mut original_key = None;
    if let Some(value) = input.get("keyfile") {
        if !value.is_null() {
            let bytes: Vec<u8> = value
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap().try_into().unwrap())
                .collect();
            let target = if input["symlink"] == true {
                home.join("synthetic-key-target")
            } else {
                key.clone()
            };
            write_private(&target, &bytes);
            if input["symlink"] == true {
                std::os::unix::fs::symlink(target.file_name().unwrap(), &key).unwrap();
            }
            original_key = Some(bytes);
        }
        if config.get("openai_api").is_none() {
            config["openai_api"] = json!({});
        }
        config["openai_api"]["api_key_file"] = json!(key);
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let port = address.port();
    let replace = |value: &str| {
        value.replace("<port>", &port.to_string()).replace(
            "<other-port>",
            &(if port < 65535 { port + 1 } else { 1024 }).to_string(),
        )
    };
    let target = replace(input["path"].as_str().unwrap_or("/v1/models"));
    let method = input["method"].as_str().unwrap_or("GET");
    let mut wire = format!("{method} {target} HTTP/1.1\r\nConnection: close\r\n");
    if let Some(hosts) = input["hosts"].as_array() {
        for host in hosts {
            wire.push_str(&format!("Host: {}\r\n", replace(host.as_str().unwrap())));
        }
    } else {
        wire.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    if let Some(headers) = input["headers"].as_array() {
        for header in headers {
            wire.push_str(&format!(
                "{}: {}\r\n",
                header[0].as_str().unwrap(),
                replace(header[1].as_str().unwrap())
            ));
        }
    }
    wire.push_str("\r\n");
    let wire: Vec<u8> = wire
        .chars()
        .map(|ch| u8::try_from(ch as u32).expect("synthetic Latin-1 wire field"))
        .collect();
    let app = Router::new().fallback(move |request: Request| {
        let config = config.clone();
        async move {
            let method = request.method().clone();
            let path = request.uri().path().trim_end_matches('/');
            let mut context = http_gate::Context::<()> {
                path: if path.is_empty() { "/" } else { path }.to_owned(),
                head: method == Method::HEAD,
                id: http_gate::random_id().unwrap(),
                cors: None,
                started: std::time::Instant::now(),
                remote: None,
            };
            if let Some(response) = http_gate::target(port, &request, &context) {
                return response;
            }
            if let Some(response) = http_gate::authorize(
                &mut context,
                port,
                request.headers(),
                method,
                request.uri().query().is_some_and(|query| !query.is_empty()),
                &config,
            ) {
                return response;
            }
            context.reply(299, json!({"policy_fixture":"admitted"}))
        }
    });
    let serving = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        transport::serve(stream, app).await;
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(&wire).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    serving.await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: std::collections::BTreeMap<_, _> = lines
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap();
            (
                key.to_ascii_lowercase(),
                value.strip_prefix(' ').unwrap_or(value),
            )
        })
        .collect();
    let mut actual = json!({"status":status,"content-type":headers.get("content-type"),
                           "body":if status==204 {Value::Null} else {serde_json::from_str(body).unwrap()},
                           "x-request-id":Value::Null});
    if let Some(id) = headers.get("x-request-id") {
        assert!(
            id.starts_with("req_")
                && id.len() == 36
                && id[4..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        actual["x-request-id"] = json!("<generated:req_32hex>");
    }
    for field in [
        "access-control-allow-origin",
        "access-control-allow-headers",
        "access-control-allow-methods",
        "access-control-allow-private-network",
        "access-control-max-age",
        "vary",
    ] {
        if let Some(value) = headers.get(field) {
            actual[field] = json!(value);
        }
    }
    // The library ends before dispatch/admission. Source setup errors and the
    // unknown local route prove these requests passed the preceding gates.
    if expected["status"].as_u64().unwrap() >= 500
        || (expected["status"] == 404 && target.starts_with("/_local/"))
    {
        expected["status"] = json!(299);
        expected["body"] = json!({"policy_fixture":"admitted"});
    }
    assert_eq!(actual, expected, "{name}");
    if let Some(bytes) = original_key {
        assert_eq!(
            std::fs::read(key).unwrap(),
            bytes,
            "key reader modified its input"
        );
    } else {
        assert!(!key.exists(), "key reader created a file");
    }
}
