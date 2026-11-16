//! Regression inputs only: no upstream requests, real credentials or ambient HOME.
//! The redactor removes the configured Key, not arbitrary unregistered token shapes.
#![cfg(feature = "secrets")]

use oxc_gateway::{redaction, secrets};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

// Intentionally realistic shapes, but entirely fake and never sent anywhere.
// The JWT's decoded subject is FAKE-TEST-ONLY and issuer is example.invalid;
// its signature is deliberately not a signature.
const CREDENTIALS: [(&str, &str); 3] = [
    ("api_key", "sk-proj-FAKE-TEST-ONLY-00000000000000000000000000000000"),
    (
        "access_token",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJGQUtFLVRFU1QtT05MWSIsImlzcyI6ImV4YW1wbGUuaW52YWxpZCJ9.RkFLRS1OT1QtQS1TSUdOQVRVUkU",
    ),
    ("refresh_token", "rt-FAKE-TEST-ONLY-00000000000000000000000000000000"),
];

struct Credentials(PathBuf);
impl Credentials {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "redaction-openai-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn key(&self, name: &str, secret: &str) -> secrets::Key {
        let path = self.0.join(format!("{name}.json"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(json!({"openai": secret}).to_string().as_bytes())
            .unwrap();
        secrets::read(&path, "openai").unwrap()
    }
}
impl Drop for Credentials {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn error_body(secret: &str) -> Value {
    json!({
        "error": {
            "message": format!("Invalid credential: {secret}"),
            "type": "authentication_error",
            "details": [secret, {"echo": format!("{secret}{secret}")}],
            "headers": {
                "authorization": format!("Bearer {secret}"),
                "x-api-key": secret,
                "cookie": format!("refresh_token={secret}"),
                format!("echo-{secret}"): "header name"
            }
        },
        "unchanged": "λ ordinary diagnostic 😀"
    })
}

fn payloads(secret: &str) -> [String; 2] {
    [
        error_body(secret).to_string(),
        format!(
            "HTTP/1.1 401 Unauthorized\r\nAuthorization: Bearer {secret}\r\n\
             X-API-Key: {secret}\r\nCookie: refresh_token={secret}\r\n\r\n\
             λ rejected {secret}{secret} 😀 {secret}"
        ),
    ]
}

#[test]
fn openai_error_values_remove_each_configured_secret_shape() {
    let credentials = Credentials::new();
    for (name, secret) in CREDENTIALS {
        let key = credentials.key(name, secret);
        let mut body = error_body(secret);
        redaction::value(&mut body, &key);
        assert_eq!(body, error_body("[redacted]"), "shape: {name}");
        assert!(!body.to_string().contains(secret), "shape: {name}");
    }
}

#[test]
fn openai_errors_and_echoed_headers_redact_whole_and_at_every_byte_boundary() {
    let credentials = Credentials::new();
    for (name, secret) in CREDENTIALS {
        let key = credentials.key(name, secret);
        for (case, payload) in payloads(secret).iter().enumerate() {
            let expected = payload.replace(secret, "[redacted]").into_bytes();
            // Includes empty first/last chunks, so split=0 also tests a whole feed.
            // Byte boundaries include the interior of multibyte Unicode text.
            for split in 0..=payload.len() {
                let mut stream = redaction::Stream::new(&key);
                let mut output = stream.feed(&payload.as_bytes()[..split]);
                output.extend(stream.feed(&payload.as_bytes()[split..]));
                output.extend(stream.finish());
                assert_eq!(output, expected, "shape: {name}, case: {case}, split: {split}");
                assert!(stream.finish().is_empty());
            }
        }
    }
}

#[test]
fn openai_errors_and_echoed_headers_redact_one_byte_chunks() {
    let credentials = Credentials::new();
    for (name, secret) in CREDENTIALS {
        let key = credentials.key(name, secret);
        for (case, payload) in payloads(secret).iter().enumerate() {
            let mut stream = redaction::Stream::new(&key);
            let mut output = Vec::new();
            for byte in payload.as_bytes() {
                output.extend(stream.feed(std::slice::from_ref(byte)));
            }
            output.extend(stream.finish());
            assert_eq!(
                output,
                payload.replace(secret, "[redacted]").as_bytes(),
                "shape: {name}, case: {case}"
            );
        }
    }
}
