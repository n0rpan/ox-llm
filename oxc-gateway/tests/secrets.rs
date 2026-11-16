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
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
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
    let replacement = home.0.join("replacement.json");
    fs::write(
        &replacement,
        br#"{"anthropic":"synthetic-next-credential"}"#,
    )
    .unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(replacement, &path).unwrap();
    let next = secrets::read(&path, "anthropic").unwrap();
    assert_eq!(first.expose(), "synthetic-first-credential");
    assert_eq!(next.expose(), "synthetic-next-credential");
    assert!(!format!("{first:?} {next:?}").contains("synthetic"));
}

#[test]
fn public_permissions_links_and_directories_are_refused() {
    let home = Home::new();
    let path = home.write(br#"{"anthropic":"synthetic-credential"}"#);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        secrets::read(&path, "anthropic").err(),
        Some(secrets::Error::Permissions)
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = home.0.join("alias.json");
    symlink(&path, &link).unwrap();
    assert_eq!(
        secrets::read(&link, "anthropic").err(),
        Some(secrets::Error::SymbolicLink)
    );
    assert_eq!(
        secrets::read(&home.0, "anthropic").err(),
        Some(secrets::Error::NotRegular)
    );
}

#[test]
fn repository_markers_have_a_distinct_refusal_without_disclosing_paths_or_keys() {
    let missing = Home::new();
    assert_eq!(
        secrets::read(&missing.0.join("missing.json"), "anthropic").err(),
        Some(secrets::Error::Unavailable)
    );
    for marker in ["git-empty", "git-head", "git-file", "source-tree"] {
        let home = Home::new();
        let path = home.write(br#"{"anthropic":"synthetic-credential"}"#);
        assert!(secrets::read(&path, "anthropic").is_ok());
        match marker {
            "git-empty" => fs::create_dir(home.0.join(".git")).unwrap(),
            "git-head" => {
                fs::create_dir(home.0.join(".git")).unwrap();
                fs::write(home.0.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            }
            "git-file" => fs::write(home.0.join(".git"), "gitdir: synthetic\n").unwrap(),
            _ => {
                fs::write(home.0.join("AGENTS.md"), "synthetic source tree\n").unwrap();
                fs::create_dir(home.0.join("engine")).unwrap();
            }
        }
        let error = secrets::read(&path, "anthropic").err().unwrap();
        assert_eq!(error, secrets::Error::InRepository);
        let message = format!("{error:?}: {error}");
        assert!(!message.contains("synthetic-credential"));
        assert!(!message.contains(path.to_str().unwrap()));
    }
}

#[test]
fn invalid_data_produces_a_clean_refusal() {
    let home = Home::new();
    for bytes in [
        b"{synthetic-invalid-json}".as_slice(),
        b"\xff",
        br#"{"anthropic":""}"#,
        br#"{"anthropic":"synthetic\ncredential"}"#,
    ] {
        let path = home.write(bytes);
        let error = secrets::read(&path, "anthropic").err().unwrap();
        assert_eq!(error, secrets::Error::Unavailable);
        assert!(!format!("{error:?}: {error}").contains("synthetic"));
    }
}

#[test]
fn valid_json_at_the_file_size_boundary_is_accepted_and_one_extra_byte_is_too_large() {
    let home = Home::new();
    let mut bytes = br#"{"anthropic":"synthetic-size-credential"}"#.to_vec();
    bytes.resize(65536, b' ');
    let path = home.write(&bytes);
    assert_eq!(
        secrets::read(&path, "anthropic").unwrap().expose(),
        "synthetic-size-credential"
    );
    bytes.push(b' ');
    home.write(&bytes);
    assert_eq!(
        secrets::read(&path, "anthropic").err(),
        Some(secrets::Error::TooLarge)
    );
}

#[test]
fn a_different_file_owner_is_refused_before_permissions_or_content() {
    use std::os::unix::fs::MetadataExt;
    // CI supplies a synthetic regular file owned by a different UID before
    // dropping capabilities. Without it, public kernel metadata provides an
    // unmapped owner in an unprivileged user namespace; no content is needed.
    let path = std::env::var_os("OXC_SECRET_OWNER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc/uptime"));
    assert_ne!(
        fs::metadata(&path).unwrap().uid(),
        nix::unistd::Uid::current().as_raw(),
        "supply a synthetic OXC_SECRET_OWNER_PATH owned by a different UID"
    );
    assert_eq!(
        secrets::read(&path, "anthropic").err(),
        Some(secrets::Error::WrongOwner)
    );
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
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "before [redacted] after"
        );
    }
}

#[test]
fn missing_relative_dangling_loop_and_parent_alias_paths_have_explicit_results() {
    let home = Home::new();
    assert_eq!(
        secrets::read(&home.0.join("missing.json"), "anthropic").err(),
        Some(secrets::Error::Unavailable)
    );
    let dangling = home.0.join("dangling.json");
    symlink(home.0.join("missing.json"), &dangling).unwrap();
    let cycle = home.0.join("cycle.json");
    symlink("cycle.json", &cycle).unwrap();
    for path in [&dangling, &cycle] {
        assert_eq!(
            secrets::read(path, "anthropic").err(),
            Some(secrets::Error::SymbolicLink)
        );
    }
    let path = home.write(br#"{"anthropic":"synthetic-parent-credential"}"#);
    let mut relative = PathBuf::new();
    for part in std::env::current_dir().unwrap().components() {
        if matches!(part, std::path::Component::Normal(_)) {
            relative.push("..");
        }
    }
    relative.push(path.strip_prefix("/").unwrap());
    assert_eq!(fs::canonicalize(&relative).unwrap(), path);
    assert!(secrets::read(&path, "anthropic").is_ok());
    assert_eq!(
        secrets::read(&relative, "anthropic").err(),
        Some(secrets::Error::Unavailable)
    );
    let actual = home.0.join("actual");
    fs::create_dir(&actual).unwrap();
    fs::rename(&path, actual.join("credentials.json")).unwrap();
    let alias = home.0.join("alias");
    symlink(&actual, &alias).unwrap();
    assert_eq!(
        secrets::read(&alias.join("credentials.json"), "anthropic")
            .unwrap()
            .expose(),
        "synthetic-parent-credential"
    );
    fs::create_dir(actual.join(".git")).unwrap();
    assert_eq!(
        secrets::read(&alias.join("credentials.json"), "anthropic").err(),
        Some(secrets::Error::InRepository)
    );
}

#[test]
fn flat_and_nested_keys_accept_the_maximum_and_reject_invalid_types_and_characters() {
    let home = Home::new();
    for raw in [
        json!({"anthropic":"synthetic-form-credential"}),
        json!({"anthropic":{"api_key":"synthetic-form-credential"}}),
    ] {
        let path = home.write(raw.to_string().as_bytes());
        assert_eq!(
            secrets::read(&path, "anthropic").unwrap().expose(),
            "synthetic-form-credential"
        );
    }
    let maximum = "p".repeat(8192);
    let path = home.write(json!({"anthropic":maximum}).to_string().as_bytes());
    assert_eq!(secrets::read(&path, "anthropic").unwrap().expose(), maximum);
    for raw in [
        json!([]),
        json!({}),
        json!({"other":"synthetic-unused"}),
        json!({"anthropic":{"api_key":true}}),
        json!({"anthropic":null}),
    ] {
        let path = home.write(raw.to_string().as_bytes());
        assert_eq!(
            secrets::read(&path, "anthropic").err(),
            Some(secrets::Error::Unavailable)
        );
    }
    for key in [
        String::new(),
        "synthetic credential".into(),
        "synthetic\ncredential".into(),
        "synthetic-λ".into(),
        "p".repeat(8193),
    ] {
        let path = home.write(json!({"anthropic":key}).to_string().as_bytes());
        let error = secrets::read(&path, "anthropic").err().unwrap();
        assert_eq!(error, secrets::Error::Unavailable);
        if !key.is_empty() {
            assert!(!error.to_string().contains(&key));
        }
    }
}

#[test]
fn utf8_json_is_accepted_and_other_encodings_or_nonfinite_values_are_refused() {
    let home = Home::new();
    let text = r#"{"anthropic":{"api_key":"synthetic-encoded-credential"}}"#;
    for bytes in [
        text.as_bytes().to_vec(),
        [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat(),
        br#"{"anthropic":"synthetic-\u0065ncoded-credential"}"#.to_vec(),
    ] {
        let path = home.write(&bytes);
        assert_eq!(
            secrets::read(&path, "anthropic").unwrap().expose(),
            "synthetic-encoded-credential"
        );
    }
    let mut formats = Vec::new();
    for little in [false, true] {
        let utf16: Vec<u8> = text
            .encode_utf16()
            .flat_map(|n| {
                if little {
                    n.to_le_bytes()
                } else {
                    n.to_be_bytes()
                }
            })
            .collect();
        let utf32: Vec<u8> = text
            .chars()
            .flat_map(|c| {
                if little {
                    (c as u32).to_le_bytes()
                } else {
                    (c as u32).to_be_bytes()
                }
            })
            .collect();
        formats.push(
            [
                if little { b"\xff\xfe" } else { b"\xfe\xff" }.as_slice(),
                &utf16,
            ]
            .concat(),
        );
        formats.push(utf16);
        formats.push(
            [
                if little {
                    b"\xff\xfe\x00\x00"
                } else {
                    b"\x00\x00\xfe\xff"
                }
                .as_slice(),
                &utf32,
            ]
            .concat(),
        );
        formats.push(utf32);
    }
    formats.push(
        br#"{"unused":[NaN,Infinity,-Infinity],"anthropic":{"api_key":"synthetic-encoded-credential"}}"#
            .to_vec(),
    );
    for bytes in formats {
        let path = home.write(&bytes);
        assert_eq!(
            secrets::read(&path, "anthropic").err(),
            Some(secrets::Error::Unavailable)
        );
    }
}

#[test]
fn streaming_buffers_partial_matches_releases_mismatches_and_preserves_unicode() {
    let home = Home::new();
    let path = home.write(br#"{"anthropic":"synthetic-stream-credential"}"#);
    let key = secrets::read(&path, "anthropic").unwrap();
    let mut stream = redaction::Stream::new(&key);
    assert_eq!(stream.feed(b"tail synthetic-st"), b"tail ");
    assert_eq!(stream.feed(b"X"), b"synthetic-stX");
    assert!(stream.finish().is_empty());
    let mut stream = redaction::Stream::new(&key);
    assert_eq!(stream.feed(b"tail syntheti"), b"tail ");
    assert_eq!(stream.finish(), b"syntheti");
    assert!(stream.finish().is_empty());
    let input = format!("λ {}{} 😀", key.expose(), key.expose());
    for split in 0..=input.len() {
        let mut stream = redaction::Stream::new(&key);
        let mut output = stream.feed(&input.as_bytes()[..split]);
        output.extend(stream.feed(&input.as_bytes()[split..]));
        output.extend(stream.finish());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "λ [redacted][redacted] 😀"
        );
    }
    let mut stream = redaction::Stream::new(&key);
    assert_eq!(stream.feed(b"ordinary text"), b"ordinary text");
    assert!(stream.feed(b"").is_empty());
    assert!(stream.finish().is_empty());
}
