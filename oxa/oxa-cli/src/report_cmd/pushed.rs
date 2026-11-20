//! The content check `oxa work --publish` applies to the commits it would push.
//! It reuses the scrubber's private-key and token patterns and adds secret-store
//! file names and the values of secret-named variables and settings.
use super::patterns;
use oxa_config::{Env, Settings};
use oxa_work::{ContentFinding, PushedContent};
use serde_json::Value;

/// Shorter values are too common to identify a secret.
const MIN_SECRET_VALUE_CHARS: usize = 8;
/// A value under a generic `_key` / `credentials` name must be at least this long: those
/// names also label ordinary short identifiers.
const MIN_KEY_VALUE_CHARS: usize = 16;
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

/// Whether the value of a `name = value` assignment, a `Bearer ..` header or a URL
/// password could be a credential rather than code or prose: long enough, free of
/// expression characters, and with a digit or a symbol (a bare identifier such as
/// `next_token` or a word such as `configuration` is not).
fn opaque_value(value: &str) -> bool {
    value.chars().count() >= MIN_SECRET_VALUE_CHARS
        && !value.contains(['(', ')', '<', '>', '{', '}', '[', ']', '&', '*', '$'])
        && value
            .chars()
            .any(|ch| ch.is_ascii_digit() || !(ch.is_alphabetic() || matches!(ch, '_' | '.' | ':')))
}

/// Whether a key names a file location (`CLIENT_SECRET_FILE`, `ssl.key.path`, `tokenDir`,
/// `GOOGLE_APPLICATION_CREDENTIALS`) rather than a secret value. Only such keys may hold a
/// relative file path; `PASSWORD`, `SECRET`, `TOKEN`, `API_KEY`, `SIGNING_KEY` and a bare
/// `CREDENTIALS` hold the secret itself, so a path-shaped value under them is refused.
fn file_location_key(name: &str) -> bool {
    let name = name.trim_matches(['"', '\'']);
    if name.eq_ignore_ascii_case("google_application_credentials") {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    let separated = ["file", "path", "dir", "directory"].iter().any(|word| {
        lower
            .strip_suffix(word)
            .is_some_and(|head| head.ends_with(['_', '-', '.']))
    });
    let camel = ["File", "Path", "Dir", "Directory"].iter().any(|word| {
        name.strip_suffix(word)
            .is_some_and(|head| head.ends_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit()))
    });
    separated || camel
}

/// A path-shaped value that is allowed under this key: a relative file path under a key that
/// names a file location.
fn location_value(name: &str, value: &str) -> bool {
    file_location_key(name) && relative_file_path(value)
}

/// A relative file path such as `config/service-account.json`: path characters only, a `/`,
/// no URL scheme and a non-empty final segment. This is used only for keys that explicitly
/// name a file location, so extensionless credential files are paths too.
fn relative_file_path(value: &str) -> bool {
    let value = value.trim().trim_matches(['"', '\'']);
    let Some((_, name)) = value.rsplit_once('/') else {
        return false;
    };
    !value.contains("://")
        && !name.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-'))
}

/// Whether a quoted value of a secret-named key is a literal credential. A quoted string is
/// data (a single-quoted shell word or a JSON/YAML string), so a `$` inside it counts as part
/// of the value; only a whole variable reference (`${NAME}`, `$(cmd)`, a double-quoted
/// `$UPPER_NAME`) or a `<placeholder>` is an expression.
fn quoted_secret_literal(text: &str) -> bool {
    static QUOTED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static REFERENCE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let quoted = QUOTED.get_or_init(|| {
        regex::Regex::new(concat!(
            r#"(?i)(?:(?:^|[^a-z])(?:api[_-]?key|access[_-]?token|auth[_-]?token|refresh[_-]?token|token|key)"#,
            r#"|secret|password|passwd|passphrase|authorization|(?-i:[a-z](?:Key|Token)))["']?\s*[:=]\s*"#,
            r#"(?:"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)')"#
        ))
        .expect("quoted secret pattern")
    });
    let reference = REFERENCE.get_or_init(|| {
        regex::Regex::new(r"^(?:\$\{[A-Za-z_][A-Za-z0-9_]*\}|\$\(.*\)|[<{\[].*[>}\]])$")
            .expect("reference pattern")
    });
    quoted.captures_iter(text).any(|found| {
        let double = found.get(1).is_some();
        let value = found.get(1).or(found.get(2)).map_or("", |m| m.as_str());
        let upper_name = double
            && value.strip_prefix('$').is_some_and(|name| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
            });
        // Spaces inside a quoted passphrase do not make it prose: only a digit or a symbol
        // other than a space marks it, so `"enter your password here"` is still not a literal.
        value.chars().count() >= MIN_SECRET_VALUE_CHARS
            && !reference.is_match(value)
            && !upper_name
            && value.chars().any(|ch| {
                ch.is_ascii_digit()
                    || !(ch.is_alphabetic() || ch.is_whitespace() || matches!(ch, '_' | '.' | ':'))
            })
    })
}

/// Distinctive token prefixes that are matched even when a word character precedes them
/// (`xghp_..`): the scrubber's `known` pattern needs a word boundary, which a token copied
/// from a project file and prefixed with a letter or digit evades.
fn embedded_known_token(text: &str) -> bool {
    static EMBEDDED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    EMBEDDED
        .get_or_init(|| {
            regex::Regex::new(concat!(
                r"gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-(?:ant|proj)-[A-Za-z0-9_-]{16,}|sk-[A-Za-z0-9]{32,}",
                r"|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}",
                r"|glpat-[A-Za-z0-9_-]{20,}|hf_[A-Za-z0-9]{30,}"
            ))
            .expect("embedded token pattern")
        })
        .is_match(text)
}

/// Whether a registry configuration line (`.npmrc`, `.pypirc`, ..) assigns a credential:
/// a comment, an empty value and an environment reference (`${NPM_TOKEN}`, which npm
/// expands at run time) hold none.
fn registry_credential_line(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with(['#', ';']) {
        return false;
    }
    let named = |text: &str| {
        ["_auth", "token", "password"]
            .iter()
            .any(|word| text.contains(word))
    };
    // `key=value` (`.npmrc`, `.pypirc`); a line without `=` (YAML-like `password: x`)
    // splits at its first colon.
    let split = line.split_once('=').or_else(|| line.split_once(':'));
    let Some((key, value)) = split else {
        return named(line);
    };
    if !named(key) {
        return false;
    }
    let value = value.trim().trim_matches(['"', '\'']);
    let reference = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|name| {
            !name.is_empty() && name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        });
    !(value.is_empty() || reference)
}

/// An unquoted environment assignment whose (possibly prefixed) name denotes a
/// credential. The shared scrubber pattern deliberately uses word boundaries,
/// which do not split `DB_PASSWORD` or `API_TOKEN`.
fn environment_secret_assignment(text: &str) -> bool {
    static ASSIGNMENT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    ASSIGNMENT
        .get_or_init(|| {
            regex::Regex::new(concat!(
                r"(?i)(?:^|[^A-Za-z0-9_])([A-Za-z_][A-Za-z0-9_]*",
                r"(?:password|passwd|passphrase|secret|token|api_?key|access_?key)",
                r"[A-Za-z0-9_]*)\s*=\s*([^\s#]+)"
            ))
            .expect("environment secret assignment pattern")
        })
        .captures_iter(text)
        .any(|found| opaque_value(&found[2]) && !location_value(&found[1], &found[2]))
}

/// An assignment (`NAME=value`, `name: value`, `"name": "value"`) whose name ends in a
/// generic key or credential word (`ENCRYPTION_KEY`, `signing.key`, `db_credentials`): the
/// shared patterns cover only `api_key`/`access_key` and need a word boundary the `_` hides.
/// `secret_name` treats these names as secret too. Such names also label ordinary values
/// (`sort_key: created_at_2`), so the value must be a long opaque literal, not a path or URL.
fn key_named_assignment(text: &str) -> bool {
    static KEYED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    KEYED
        .get_or_init(|| {
            regex::Regex::new(concat!(
                r#"(?i)(?:^|[^a-z0-9_.-])["']?([a-z0-9_.-]*(?:[_.-]key|credentials?))["']?\s*[:=]\s*"#,
                r#"["']?([^\s"'#,;]+)"#
            ))
            .expect("key-named assignment pattern")
        })
        .captures_iter(text)
        .any(|found| {
            let value = found[2].trim();
            value.chars().count() >= MIN_KEY_VALUE_CHARS
                && opaque_value(value)
                && !value.starts_with(['/', '~', '.'])
                && !location_value(&found[1], value)
                && !value.contains("://")
        })
}

/// `Authorization: Basic …` / `Bearer …` with an encoded value: base64 of a user and a password
/// may be letters only, which the class-mixing heuristic of `opaque_value` rejects.
fn scheme_credential(text: &str) -> bool {
    static SCHEME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    SCHEME
        .get_or_init(|| {
            regex::Regex::new(
                r#"(?i)authorization["']?\s*[:=]\s*["']?(?:basic|bearer)\s+([A-Za-z0-9+/=._-]{8,})"#,
            )
            .expect("authorization scheme pattern")
        })
        .captures_iter(text)
        .any(|found| {
            let value = found[1].trim();
            !value.starts_with('$') && !value.contains("${")
        })
}

/// A YAML/TOML assignment whose key merely *contains* a credential word: `database_password:`
/// or `svc.api_token =` — the shared keyed pattern needs a boundary the underscore hides.
fn prefixed_assignment(text: &str) -> bool {
    static PREFIXED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PREFIXED
        .get_or_init(|| {
            regex::Regex::new(concat!(
                r#"(?i)(?:^|[^a-z0-9_])["']?([a-z0-9_.-]*(?:api[_-]?key|access[_-]?token|auth[_-]?token"#,
                r#"|refresh[_-]?token|token|secret|password|passwd|passphrase)[a-z0-9_.-]*)["']?\s*[:=]\s*["']?([^\s"'#,;]+)"#
            ))
            .expect("prefixed assignment pattern")
        })
        .captures_iter(text)
        .any(|found| opaque_value(found[2].trim()) && !location_value(&found[1], &found[2]))
}

/// A PuTTY private key is a line-oriented format, not PEM: `PuTTY-User-Key-File-N` and
/// `Private-MAC` appear in a copied key under any file name.
/// An age identity is a bare `AGE-SECRET-KEY-…` line, so a copy without its `.config/sops/age`
/// path is otherwise invisible; the PQ format uses the same words with a `PQ-` market.
fn age_identity(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .any(|word| {
            let upper = word.to_ascii_uppercase();
            upper.starts_with("AGE-SECRET-KEY-1") || upper.starts_with("AGE-SECRET-KEY-PQ-")
        })
}

fn putty_key(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("PuTTY-User-Key-File-")
            || line.starts_with("Private-Lines:")
            || line.starts_with("Private-MAC:")
    })
}

/// A JSON credential key at the end of one added line with its value on the next:
/// `"password":` / `"opaque-pass-value-17"`. Each line alone looks harmless.
fn json_split_secret(text: &str) -> bool {
    static SPLIT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    SPLIT
        .get_or_init(|| {
            regex::Regex::new(
                r#"(?i)"[^"\n]*(?:password|passwd|passphrase|secret|token|api_?key|access_?key)[^"\n]*"\s*:\s*\n\s*"([^"]+)""#,
            )
            .expect("JSON split secret pattern")
        })
        .captures_iter(text)
        .any(|found| opaque_value(found[1].trim()))
}

/// A YAML secret key followed by a block-scalar marker and one of its added
/// value lines. `scan_patch` supplies this bounded two-line context.
fn block_scalar_secret(text: &str) -> bool {
    static BLOCK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    BLOCK
        .get_or_init(|| {
            regex::Regex::new(concat!(
                r#"(?i)(?:^|[^a-z])"?[^"\n]*(?:api[_-]?key|access[_-]?token|auth[_-]?token|refresh[_-]?token|token"#,
                r#"|secret|password|passwd|passphrase|authorization|key)"?\s*:\s*[|>](?:[+-]?\d?|\d[+-])?\s*(?:#[^\n]*)?\n[^\n]*?([^\s].*)$"#
            ))
            .expect("YAML secret block pattern")
        })
        .captures_iter(text)
        .any(|found| opaque_value(found[1].trim()))
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

/// Whether `path` lies in a credential location of the sandbox's private-path inventory
/// (`.docker/config.json`, `.aws`, `.kube`, ..), as a project-local copy of it. Registry
/// files stay with the content check, which tells their credentials from plain settings.
fn inventory_store(path: &str) -> bool {
    let parts: Vec<String> = path.split('/').map(|part| part.to_ascii_lowercase()).collect();
    oxa_proto::guard::SENSITIVE_HOME.iter().any(|entry| {
        let entry: Vec<String> = entry.split('/').map(|part| part.to_ascii_lowercase()).collect();
        let registry = entry.len() == 1 && REGISTRY_FILES.contains(&entry[0].as_str());
        !registry
            && parts
                .windows(entry.len())
                .any(|window| window == entry.as_slice())
    })
}

/// A JSON auth field of a container registry login (`"auth": "<base64 user:password>"`).
fn registry_auth(text: &str) -> bool {
    static AUTH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    AUTH.get_or_init(|| {
        regex::Regex::new(r#"(?i)"(?:auth|identitytoken|registrytoken)"\s*:\s*"([A-Za-z0-9+/=_.-]+)""#)
            .expect("registry auth pattern")
    })
    .captures_iter(text)
    .any(|found| {
        // A login's base64 may be letters only (`user:passphrase` encodes without digits), which
        // the class-mixing heuristic rejects; a template placeholder is not a credential.
        let value = found[1].trim();
        value.len() >= 8 && !value.starts_with('$') && !value.contains("${")
    })
}

fn env_template(path: &str) -> bool {
    let name = basename(path);
    [".example", ".sample", ".template", ".dist"]
        .iter()
        .any(|end| name.starts_with(".env.") && name.ends_with(end))
}

fn secret_store(path: &str) -> bool {
    let name = basename(path);
    inventory_store(path)
        || name == ".env"
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
        // A file with no added line (an empty file) holds no secret, so its name alone is
        // not a finding; every file with content reaches the line checks below.
        item.line?;
        if let Some(label) = self.secret_text(item.text) {
            return Some(found(label));
        }
        // An existing tracked secret store gains content as well as a new one.
        // A template (`.env.example`) is only refused for a secret its content holds.
        if secret_store(item.path) && !env_template(item.path) {
            return Some(found("content to a file name used for secrets"));
        }
        if registry_auth(item.text) {
            return Some(found("a registry login auth field"));
        }
        let registry = REGISTRY_FILES.contains(&basename(item.path).as_str());
        let line = item.text.to_ascii_lowercase();
        if registry && registry_credential_line(&line) {
            return Some(found("a package registry credential"));
        }
        None
    }

    fn secret_text(&self, text: &str) -> Option<&'static str> {
        let p = patterns();
        if p.pem_begin.is_match(text) || putty_key(text) || age_identity(text) {
            return Some("a private key");
        }
        if scheme_credential(text) {
            return Some("an authorization credential");
        }
        if p.known.is_match(text) || embedded_known_token(text) {
            return Some("a known access token format");
        }
        // The scrubber's keyed, `Bearer ..` and URL-credential patterns, which catch
        // an opaque credential copied from a project file that no configured value matches.
        let value_of = |text: &str| text.split_whitespace().last().unwrap_or_default().to_string();
        if p.keyed.captures_iter(text).any(|found| opaque_value(&found[2]))
            || quoted_secret_literal(text)
            || environment_secret_assignment(text)
            || prefixed_assignment(text)
            || key_named_assignment(text)
            || registry_auth(text)
            || block_scalar_secret(text)
            || json_split_secret(text)
            || p.bearer
                .find_iter(text)
                .any(|found| opaque_value(&value_of(found.as_str())))
            || p.url_cred.find_iter(text).any(|found| {
                let userinfo = found.as_str().trim_end_matches('@');
                userinfo
                    .rsplit_once(':')
                    .is_some_and(|(_, password)| opaque_value(password))
            })
        {
            return Some("a credential assignment, authorization header or URL password");
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
    fn added<'a>(path: &'a str, text: &'a str) -> PushedContent<'a> {
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

    /// Three copy-and-rename forms the scanner missed: an unquoted prefixed YAML key, a JSON
    /// value whose escaped quote truncated the captured literal, and a PuTTY key under a plain
    /// file name.
    #[test]
    fn prefixed_yaml_escaped_quotes_and_putty_keys_are_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for text in [
            "database_password: opaque-pass-value-17",
            "svc.api_token = opaque-pass-value-17",
            r#"{"password": "a\"opaque-pass-value-17"}"#,
            "PuTTY-User-Key-File-3: ssh-rsa",
        ] {
            assert!(
                scan.check(&added("notes.txt", text)).is_some(),
                "not refused: {text}"
            );
        }
        // A placeholder is still allowed, and so is a Basic credential's placeholder.
        assert!(scan
            .check(&added("notes.txt", "database_password: ${DB_PASSWORD}"))
            .is_none());
        assert!(scan
            .check(&added("notes.txt", "Authorization: Basic ${BASIC}"))
            .is_none());
        // An age identity and a registry login renamed to a plain file.
        assert!(
            scan.check(&added(
                "notes.txt",
                "AGE-SECRET-KEY-1QJ0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0J0"
            ))
            .is_some(),
            "an age identity was not refused"
        );
        assert!(scan
            .check(&added(
                "registry-config.json",
                r#"{"auths":{"registry.example.invalid":{"auth":"dXNlcjpwYXNzcGhyYXNl"}}}"#
            ))
            .is_some());
        // Base64 of `user:passphrase` is letters only; the class mixer would miss it.
        assert!(scan
            .check(&added("notes.txt", "Authorization: Basic dXNlcjpwYXNzcGhyYXNl"))
            .is_some());
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
            // An empty file holds no secret; a non-empty one is refused by its lines.
            assert!(scan.check(&file(name)).is_none(), "{name}");
            assert!(scan.check(&line(name, "PLAIN=value")).is_some(), "{name}");
        }
        let token_name = format!("notes/ghp_{}.txt", "x".repeat(36));
        let finding = scan.check(&file(&token_name)).unwrap();
        assert!(finding.secret_path);
        for name in ["src/env.rs", "id_ed25519.pub", "keyboard.rs", "README.md"] {
            assert!(scan.check(&file(name)).is_none(), "{name}");
        }
    }

    #[test]
    fn empty_secret_store_file_publishes() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for name in [".env", "deploy/server.pem", ".netrc", "aws/credentials"] {
            assert!(scan.check(&file(name)).is_none(), "{name}");
        }
    }

    #[test]
    fn env_example_without_secrets_publishes() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for name in [".env.example", "app/.env.sample", ".env.template"] {
            for text in [
                "PORT=8080",
                "API_TOKEN=",
                "DB_PASSWORD=<your-password>",
                "# copy to .env",
            ] {
                assert!(scan.check(&line(name, text)).is_none(), "{name} {text}");
            }
        }
    }

    #[test]
    fn env_example_with_a_token_is_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        let token = format!("API_TOKEN=ghp_{}", "x".repeat(36));
        let password = "PASSWORD=opaque-pass-value-17".to_string();
        for text in [token, password] {
            assert!(scan.check(&added(".env.example", &text)).is_some());
        }
        // A real `.env` still refuses plain lines.
        assert!(scan.check(&line(".env", "PORT=8080")).is_some());
    }

    #[test]
    fn quoted_literal_with_spaces_is_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for text in [
            "\"password\": \"synthetic passphrase 17\"",
            "PASSWORD='correct horse battery #7'",
            "token: \"two words 42\"",
        ] {
            assert!(scan.check(&added("notes.json", text)).is_some(), "{text}");
        }
        for text in [
            "\"password\": \"enter your password here\"",
            "password: \"<your password 1>\"",
        ] {
            assert!(scan.check(&added("notes.json", text)).is_none(), "{text}");
        }
    }

    #[test]
    fn known_token_prefix_is_matched_after_word_characters() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        let github = format!("x{}{}", "ghp_", "a1".repeat(18));
        let pat = format!("v{}{}", "github_pat_", "A".repeat(30));
        let aws = format!("Z{}{}", "AKIA", "X".repeat(16));
        let slack = format!("q{}{}", "xoxb-", "1".repeat(12));
        let hugging_face = format!("x{}{}", "hf_", "a1".repeat(15));
        for text in [github, pat, aws, slack, hugging_face] {
            assert!(scan.check(&added("notes.txt", &text)).is_some());
        }
        // Ordinary words that contain a prefix are not tokens.
        for text in [
            "disk-encryption-configuration-guide",
            "ghp_short",
            "taskforce-AKIA-list",
        ] {
            assert!(scan.check(&line("notes.txt", text)).is_none(), "{text}");
        }
    }

    #[test]
    fn project_local_docker_login_is_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        // base64 of a synthetic `user:password`, built at run time.
        let auth = format!("{}{}", "dXNlcjpwYXNz", "d29yZA==");
        let entry = format!("{{\"auths\":{{\"registry.example.invalid\":{{\"auth\":\"{auth}\"}}}}}}");
        // An empty copy holds nothing; any content in them is refused.
        assert!(scan.check(&file(".docker/config.json")).is_none());
        for path in [
            ".docker/config.json",
            "deploy/.docker/config.json",
            ".kube/config",
        ] {
            assert!(scan.check(&line(path, "{}")).is_some(), "{path}");
        }
        assert!(scan.check(&added(".docker/config.json", &entry)).is_some());
        // The field is recognised in any file, but a plain word or a package file is not.
        assert!(scan
            .check(&added("notes.json", &format!("\"auth\": \"{auth}\"")))
            .is_some());
        assert!(scan.check(&line("notes.json", "\"auth\": \"basic\"")).is_none());
        assert!(scan.check(&file("docker/config.json")).is_none());
        assert!(scan.check(&file(".npmrc")).is_none());
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
    fn relative_credential_file_paths_pass_in_templates() {
        let scan = PushScan::new(
            &Env::from_pairs(&[]),
            &Settings::from_values(json!({}), json!({})),
        );
        for text in [
            "GOOGLE_APPLICATION_CREDENTIALS=config/service-account.json",
            "GOOGLE_APPLICATION_CREDENTIALS=config/service-account",
            "SIGNING_KEY_FILE=secrets/dev/signing-key.pem",
            "service.tls.keyPath: certs/prod/tls.key",
            "SECRETS_DIR=secrets/dev/store.d",
            "CLIENT_SECRET_FILE=conf/oauth/client-secret.json",
            "service_secret_path: certs/prod/tls.key",
        ] {
            assert!(scan.check(&added(".env.example", text)).is_none(), "{text}");
            assert!(
                scan.check(&added("deploy/app/.env.example", text)).is_none(),
                "{text}"
            );
        }
        for text in [
            "ENCRYPTION_KEY=Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MA/b64==",
            "GOOGLE_APPLICATION_CREDENTIALS=abcdEFGH1234ijklMNOP5678",
        ] {
            assert!(scan.check(&added(".env.example", text)).is_some(), "{text}");
        }
        // A path-shaped value under a key that names the secret itself is still a credential.
        for text in [
            "PASSWORD=abc/def.txt",
            "DB_PASSWORD=abc/def.txt",
            "SECRET=abc/def.txt",
            "CLIENT_SECRET=abc/def.txt",
            "API_TOKEN=abc/def.txt",
            "token: abc/def.txt",
            "API_KEY=abc/def.txt",
            "SIGNING_KEY=secrets/dev/signing-key.pem",
            "PRIVATE_KEY=abc/def-ghi-jkl.pem",
            "DB_CREDENTIALS=abc/def-ghi-jkl.txt",
            "passwd=abc/def.txt",
        ] {
            assert!(scan.check(&added(".env.example", text)).is_some(), "{text}");
            assert!(
                scan.check(&added("deploy/app/.env.example", text)).is_some(),
                "{text}"
            );
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
    fn scrubber_credential_patterns_are_refused_in_added_lines() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for text in [
            "PASSWORD=opaque-pass-value-17",
            "\"api_key\": \"abcd1234efgh\"",
            "Authorization: Bearer abcd1234efgh5678",
            "url = https://deploy:s3cr3t-pass-9@example.invalid/repo.git",
        ] {
            let found = scan.check(&line("notes.txt", text)).expect(text);
            assert!(!found.secret_path, "{text}");
        }
        for text in [
            "let token = next_token();",
            "token: String,",
            "password = read_password(prompt)",
            "see the basic configuration guide",
            "https://user:${PASSWORD}@example.invalid/",
            "Bearer {token}",
        ] {
            assert!(scan.check(&line("src/main.rs", text)).is_none(), "{text}");
        }
    }

    #[test]
    fn prefixed_environment_assignments_are_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for text in [
            "DB_PASSWORD=opaque-pass-value-17",
            "API_TOKEN=opaque-pass-value-17",
            "SERVICE_API_KEY=opaque-pass-value-17",
        ] {
            assert!(scan.check(&line(".env.example", text)).is_some());
        }
        for text in [
            "API_TOKEN=${TOKEN}",
            "DB_PASSWORD=<password>",
            "TOKEN=placeholder",
        ] {
            assert!(scan.check(&line(".env.example", text)).is_none());
        }
    }

    /// Audit of #1264: a generic `_KEY` / `credentials` name (which `secret_name` already
    /// treats as secret) and a quoted `"Authorization"` JSON key are refused like the other
    /// credential assignments; short identifiers, paths and references under such names are not.
    #[test]
    fn generic_key_names_and_quoted_authorization_keys_are_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        for (path, text) in [
            (".env.example", "ENCRYPTION_KEY=opaque-pass-value-17"),
            ("notes.txt", "export SIGNING_KEY=opaque-pass-value-17"),
            ("settings.yml", "jwt.signing-key: opaque-pass-value-17"),
            ("settings.json", "{\"db_credentials\": \"opaque-pass-value-17\"}"),
            (
                "settings.json",
                "{\"Authorization\": \"Basic dXNlcjpwYXNzcGhyYXNl\"}",
            ),
            ("settings.yml", "'authorization': 'Bearer dXNlcjpwYXNzcGhyYXNl'"),
        ] {
            assert!(scan.check(&added(path, text)).is_some(), "{path}: {text}");
        }
        for (path, text) in [
            (".env.example", "ENCRYPTION_KEY=${ENCRYPTION_KEY}"),
            (".env.example", "ENCRYPTION_KEY=<encryption-key>"),
            ("settings.yml", "sort_key: created_at_2"),
            (
                "settings.yml",
                "google_application_credentials: /etc/gcp/service-account.json",
            ),
            ("settings.yml", "ssh_key: ~/.ssh/id_deploy_ed25519.pub"),
            ("settings.json", "{\"monkey\":\"opaque-pass-value-17\"}"),
        ] {
            assert!(scan.check(&added(path, text)).is_none(), "{path}: {text}");
        }
    }

    #[test]
    fn suffix_named_quoted_keys_and_prefixed_generic_tokens_are_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        let generic = format!("xsk-{}", "a1".repeat(24));
        for text in [
            "{\"databasePassword\":\"opaque-pass-value-17\"}".to_string(),
            "{\"serviceToken\": \"opaque-pass-value-17\"}".to_string(),
            "dbSecret: 'opaque-pass-value-17'".to_string(),
            format!("let t = \"{generic}\";"),
        ] {
            assert!(scan.check(&added("config.json", &text)).is_some(), "{text}");
        }
        assert!(scan
            .check(&added("config.json", "{\"monkey\":\"opaque-value-17\"}"))
            .is_none());
    }

    #[test]
    fn yaml_block_scalar_context_is_refused() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        assert!(scan
            .check(&line("settings.yml", "password: |-\n  opaque-pass-value-17"))
            .is_some());
        assert!(scan
            .check(&line("settings.yml", "description: |-\n  opaque-pass-value-17"))
            .is_none());
    }

    #[test]
    fn quoted_literals_with_a_dollar_are_refused_but_variable_expressions_are_not() {
        let scan = PushScan::new(&Env::default(), &Settings::from_values(json!({}), json!({})));
        let d = "$";
        let literals = [
            format!("\"password\": \"opaque{d}pass17\""),
            format!("PASSWORD='{d}ecr3t-pass-9'"),
            format!("token: '{d}ecr3t12345'"),
            format!("DB_PASSWORD=\"{d}ecr3t_pass\""),
            format!("{{\"secret\":\"{d}{{x}}opaque17\"}}"),
        ];
        for text in &literals {
            assert!(scan.check(&added("notes.json", text)).is_some(), "{text}");
        }
        let expressions = [
            format!("PASSWORD={d}DB_PASSWORD"),
            format!("PASSWORD={d}{{DB_PASSWORD}}"),
            format!("PASSWORD={d}(pass show db)"),
            format!("\"password\": \"{d}{{DB_PASSWORD}}\""),
            format!("PASSWORD=\"{d}DB_PASSWORD\""),
            format!("token: \"{d}(vault read token)\""),
            "password: \"<your-password-1>\"".to_string(),
        ];
        for text in &expressions {
            assert!(scan.check(&added("src/main.rs", text)).is_none(), "{text}");
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
        for text in [
            "//registry.example.invalid/:_authToken=${NPM_TOKEN}",
            "; the token is read from the environment",
            "# password goes here",
        ] {
            assert!(scan.check(&line(".npmrc", text)).is_none(), "{text}");
        }
        for text in ["let token = next_token();", "password: String,", "vi"] {
            assert!(scan.check(&line("src/main.rs", text)).is_none(), "{text}");
        }
    }
}
