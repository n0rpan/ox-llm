//! The content check `oxa work --publish` applies to the commits it would push.
//! It reuses the scrubber's private-key and token patterns and adds secret-store
//! file names and the values of secret-named variables and settings.
use super::patterns;
use oxa_config::{Env, Settings};
use oxa_work::{ContentFinding, PushedContent};
use serde_json::Value;

/// Shorter values are too common to identify a secret.
const MIN_SECRET_VALUE_CHARS: usize = 8;
const KEY_FILE_PREFIXES: [&str; 4] = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
const KEY_FILE_SUFFIXES: [&str; 7] = [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".ppk"];
const REGISTRY_FILES: [&str; 4] = [".npmrc", ".pypirc", ".yarnrc", ".yarnrc.yml"];

/// Whether a variable or setting name marks its value as secret. Names are matched by
/// substring because `\b` does not separate words joined by `_` (`GITHUB_TOKEN`).
fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "passphrase",
        "apikey",
        "api_key",
        "api-key",
        "authorization",
        "credential",
        "private_key",
        "privatekey",
        "access_key",
    ]
    .iter()
    .any(|word| name.contains(word))
        || name == "key"
        || [".key", "_key", "-key"].iter().any(|end| name.ends_with(end))
}

/// Whether a secret-named variable or setting names a location (`SSH_KEY_PATH`,
/// `tokenFile`) rather than holding the secret itself.
fn path_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["path", "file", "dir", "directory"]
        .iter()
        .any(|end| name.ends_with(end))
}

fn secret_value(value: &str) -> Option<String> {
    let value = value.trim();
    (value.chars().count() >= MIN_SECRET_VALUE_CHARS).then(|| value.to_string())
}

fn setting_values(name: &str, value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                setting_values(key, value, out);
            }
        }
        Value::Array(rows) => rows.iter().for_each(|row| setting_values(name, row, out)),
        Value::String(text) if secret_name(name) && !path_name(name) => out.extend(secret_value(text)),
        _ => {}
    }
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

fn secret_store(path: &str) -> bool {
    let name = basename(path);
    name == ".env"
        || name.ends_with(".env")
        || name.starts_with(".env.")
        || name.contains("credentials")
        || matches!(name.as_str(), ".netrc" | "_netrc" | ".pgpass" | ".htpasswd")
        || KEY_FILE_SUFFIXES.iter().any(|end| name.ends_with(end))
        || KEY_FILE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix) && !name.ends_with(".pub"))
}

/// Refuses content that the scrubber treats as secret before `oxa work` pushes it.
pub(crate) struct PushScan {
    values: Vec<String>,
}

impl PushScan {
    /// The secret-named environment variables and settings with values long enough to
    /// identify them.
    pub(crate) fn new(env: &Env, settings: &Settings) -> PushScan {
        let mut values: Vec<String> = env
            .pairs()
            .iter()
            .filter(|(name, _)| secret_name(name) && !path_name(name))
            .filter_map(|(_, value)| secret_value(value))
            .collect();
        setting_values("", settings.merged(), &mut values);
        // Added content is scanned line by line, so a multiline value is matched
        // through each of its lines.
        let values = values.iter().flat_map(|value| {
            if value.contains('\n') {
                value.lines().filter_map(secret_value).collect()
            } else {
                vec![value.clone()]
            }
        });
        let mut values: Vec<String> = values.collect();
        values.sort();
        values.dedup();
        PushScan { values }
    }

    /// The category of secret `item` adds, if any. The result never contains the content.
    pub(crate) fn check(&self, item: &PushedContent<'_>) -> Option<ContentFinding> {
        // A path that itself contains secret text is refused and withheld for every
        // event, including lines added to an existing file.
        if let Some(label) = self.secret_text(item.path) {
            return Some(ContentFinding {
                label,
                secret_path: true,
            });
        }
        let found = |label| ContentFinding {
            label,
            secret_path: false,
        };
        if item.line.is_none() {
            return secret_store(item.path).then(|| found("a file name used for secrets"));
        }
        if let Some(label) = self.secret_text(item.text) {
            return Some(found(label));
        }
        // An existing tracked secret store gains content as well as a new one.
        if secret_store(item.path) {
            return Some(found("content to a file name used for secrets"));
        }
        let registry = REGISTRY_FILES.contains(&basename(item.path).as_str());
        let line = item.text.to_ascii_lowercase();
        if registry
            && ["_auth", "token", "password"]
                .iter()
                .any(|word| line.contains(word))
        {
            return Some(found("a package registry credential"));
        }
        None
    }

    fn secret_text(&self, text: &str) -> Option<&'static str> {
        let p = patterns();
        if p.pem_begin.is_match(text) {
            return Some("a private key");
        }
        if p.known.is_match(text) {
            return Some("a known access token format");
        }
        if self.values.iter().any(|value| text.contains(value.as_str())) {
            return Some("the value of a secret environment variable or setting");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(path: &'static str, text: &'static str) -> PushedContent<'static> {
        PushedContent {
            path,
            line: Some(1),
            text,
        }
    }
    fn file(path: &str) -> PushedContent<'_> {
        PushedContent {
            path,
            line: None,
            text: "",
        }
    }

    #[test]
    fn secret_store_and_token_names_are_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for name in [
            ".env",
            "app/.env.local",
            "prod.env",
            "deploy/server.pem",
            "tls.key",
            "home/id_ed25519",
            ".git-credentials",
            "aws/credentials",
            ".netrc",
        ] {
            assert!(scan.check(&file(name)).is_some(), "{name}");
        }
        assert!(scan.check(&file(".env.example")).is_some());
        let token_name = format!("notes/ghp_{}.txt", "x".repeat(36));
        let finding = scan.check(&file(&token_name)).unwrap();
        assert!(finding.secret_path);
        assert!(!scan.check(&file(".env")).unwrap().secret_path);
        for name in ["src/env.rs", "id_ed25519.pub", "keyboard.rs", "README.md"] {
            assert!(scan.check(&file(name)).is_none(), "{name}");
        }
    }

    #[test]
    fn lines_added_to_an_existing_secret_store_are_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for path in [".env", "deploy/aws/credentials", "config/.env.production"] {
            let item = PushedContent {
                path,
                line: Some(2),
                text: "PLAIN=value",
            };
            assert!(scan.check(&item).is_some(), "{path}");
        }
        assert!(scan.check(&line("src/main.rs", "PLAIN=value")).is_none());
    }

    #[test]
    fn a_secret_path_is_withheld_for_added_lines() {
        let configured = format!("synthetic{}", "Q".repeat(12));
        let env = Env::from_pairs(&[("SYNTHETIC_API_TOKEN", &configured)]);
        let scan = PushScan::new(&env, &Settings::from_values(json!({}), json!({})));
        let token_path = format!("notes/ghp_{}.txt", "x".repeat(36));
        let value_path = format!("notes/{configured}.txt");
        let copied = format!("copied ghp_{}", "x".repeat(36));
        for path in [token_path.as_str(), value_path.as_str()] {
            for text in ["plain text", copied.as_str()] {
                let item = PushedContent {
                    path,
                    line: Some(3),
                    text,
                };
                assert!(scan.check(&item).unwrap().secret_path, "{path}");
            }
        }
    }

    #[test]
    fn path_like_secret_values_are_matched_unless_named_as_locations() {
        let value = format!("/{}", "s".repeat(20));
        let home = format!("~{}", "t".repeat(20));
        let location = format!("/srv/{}", "u".repeat(20));
        let env = Env::from_pairs(&[
            ("SYNTHETIC_TOKEN", &value),
            ("SYNTHETIC_PASSWORD", &home),
            ("SYNTHETIC_KEY_PATH", &location),
        ]);
        let settings = Settings::from_values(
            json!({"service":{"tokenFile": "/srv/not-a-secret-value"}}),
            json!({}),
        );
        let scan = PushScan::new(&env, &settings);
        for text in [format!("a {value}"), format!("b {home}")] {
            let item = PushedContent {
                path: "src/main.rs",
                line: Some(1),
                text: &text,
            };
            assert!(scan.check(&item).is_some(), "{text}");
        }
        for text in [format!("c {location}"), "d /srv/not-a-secret-value".to_string()] {
            let item = PushedContent {
                path: "src/main.rs",
                line: Some(1),
                text: &text,
            };
            assert!(scan.check(&item).is_none(), "{text}");
        }
    }

    #[test]
    fn multiline_secret_values_are_matched_line_by_line() {
        let value = "synthetic-first-half\nsynthetic-second-half";
        let env = Env::from_pairs(&[("SYNTHETIC_PASSWORD", value)]);
        let scan = PushScan::new(&env, &Settings::from_values(json!({}), json!({})));
        for text in ["synthetic-first-half", "x synthetic-second-half"] {
            assert!(scan.check(&line("notes.txt", text)).is_some(), "{text}");
        }
    }

    #[test]
    fn multiword_secret_values_are_matched() {
        let passphrase = format!("synthetic {} phrase words", "q".repeat(6));
        let env = Env::from_pairs(&[("SYNTHETIC_PASSPHRASE", &passphrase)]);
        let scan = PushScan::new(&env, &Settings::from_values(json!({}), json!({})));
        let text = format!("note: {passphrase}");
        let item = PushedContent {
            path: "src/main.rs",
            line: Some(1),
            text: &text,
        };
        assert!(scan.check(&item).is_some());
    }

    #[test]
    fn token_shapes_private_keys_and_configured_values_are_refused() {
        let token = format!("ghp_{}", "x".repeat(36));
        let anthropic = format!("sk-ant-{}", "x".repeat(24));
        let aws = format!("AKIA{}", "X".repeat(16));
        let slack = format!("xoxb-{}", "1".repeat(12));
        let pem = format!("-----BEGIN {}PRIVATE KEY-----", "OPENSSH ");
        let configured = format!("synthetic{}", "Q".repeat(12));
        let env = Env::from_pairs(&[("SYNTHETIC_API_TOKEN", &configured), ("EDITOR", "vi")]);
        let settings = Settings::from_values(json!({"service":{"apiKey":"settingvalue1"}}), json!({}));
        let scan = PushScan::new(&env, &settings);
        for text in [
            format!("let t = \"{token}\";"),
            format!("key = {anthropic}"),
            aws,
            slack,
            pem,
            format!("value: {configured}"),
            "use settingvalue1 here".to_string(),
        ] {
            let item = PushedContent {
                path: "src/main.rs",
                line: Some(3),
                text: &text,
            };
            let finding = scan.check(&item).expect("refused");
            assert!(!finding.label.contains(&text), "{finding:?}");
        }
        let npmrc = format!("//registry.example.invalid/:_authToken={}", "z".repeat(8));
        let item = PushedContent {
            path: ".npmrc",
            line: Some(1),
            text: &npmrc,
        };
        assert!(scan.check(&item).is_some());
        for text in ["let token = next_token();", "password: String,", "vi"] {
            assert!(scan.check(&line("src/main.rs", text)).is_none(), "{text}");
        }
    }
}
