#![cfg(feature = "secrets")]
use oxc_gateway::{redaction, secrets};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gateway-security-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn write(&self, bytes: &[u8]) -> PathBuf {
        let path = self.0.join("credentials.json");
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn credentials_are_fresh_and_debug_never_discloses_them() {
    let home = Home::new();
    let path = home.write(br#"{"anthropic":"synthetic-first-credential"}"#);
    let first = secrets::read(&path, "anthropic").unwrap();
    home.write(br#"{"anthropic":"synthetic-next-credential"}"#);
    let next = secrets::read(&path, "anthropic").unwrap();
    assert_eq!(first.expose(), "synthetic-first-credential");
    assert_eq!(next.expose(), "synthetic-next-credential");
    assert!(!format!("{first:?} {next:?}").contains("synthetic"));
}

#[test]
fn public_permissions_links_directories_and_source_trees_are_refused() {
    let home = Home::new();
    let path = home.write(br#"{"anthropic":"synthetic-credential"}"#);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(secrets::read(&path, "anthropic").is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = home.0.join("alias.json");
    symlink(&path, &link).unwrap();
    assert!(secrets::read(&link, "anthropic").is_err());
    assert!(secrets::read(&home.0, "anthropic").is_err());
    fs::create_dir(home.0.join(".git")).unwrap();
    assert!(secrets::read(&path, "anthropic").is_err());
}

#[test]
fn invalid_or_oversized_data_produces_a_clean_refusal() {
    let home = Home::new();
    for bytes in [
        b"{synthetic-invalid-json}".as_slice(),
        b"\xff",
        br#"{"anthropic":""}"#,
        br#"{"anthropic":"synthetic\ncredential"}"#,
    ] {
        let path = home.write(bytes);
        let error = secrets::read(&path, "anthropic").err().unwrap();
        assert!(!format!("{error:?}: {error}").contains("synthetic"));
    }
    let path = home.write(&vec![b'x'; 65537]);
    assert!(secrets::read(&path, "anthropic").is_err());
}

#[test]
fn decoded_nested_keys_and_values_are_redacted_before_encoding() {
    let home = Home::new();
    let secret = "synthetic\"credential\\value";
    let path = home.write(json!({"anthropic":secret}).to_string().as_bytes());
    let key = secrets::read(&path, "anthropic").unwrap();
    let mut value = json!({secret:[secret,{"nested":format!("prefix {secret} suffix")}]});
    redaction::value(&mut value, &key);
    assert_eq!(
        value,
        json!({"[redacted]":["[redacted]",{"nested":"prefix [redacted] suffix"}]})
    );
}

#[test]
fn streaming_redaction_catches_every_split_of_the_credential() {
    let home = Home::new();
    let path = home.write(br#"{"anthropic":"synthetic-stream-credential"}"#);
    let key = secrets::read(&path, "anthropic").unwrap();
    let input = format!("before {} after", key.expose());
    for split in 0..=input.len() {
        let mut stream = redaction::Stream::new(&key);
        let mut output = stream.feed(&input.as_bytes()[..split]);
        output.extend(stream.feed(&input.as_bytes()[split..]));
        output.extend(stream.finish());
        assert_eq!(String::from_utf8(output).unwrap(), "before [redacted] after");
    }
}
