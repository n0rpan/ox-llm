//! Static lint of the ox AI panel source (`oxa/omarchy-panel`), run as an ordinary oxa-cli test.
//! The panel is QML and JavaScript that Quickshell runs with the user's rights, so these checks
//! keep its surface small: no network, no file reads, no key or ledger paths, one program per
//! process, no hex colours (the theme owns every colour), and only translation keys that exist.

use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Files allowed to declare a `Process`. P4 moves the processes into Feeds.qml; remove
/// "Panel.qml" from this list in that slice so the lint holds the panel to Feeds.qml alone.
const PROCESS_FILES: &[&str] = &["Feeds.qml", "Panel.qml"];

/// Lowercase text that must never appear in the panel: key stores and the cloud spend ledger.
const FORBIDDEN_TEXT: &[&str] = &["secrets", "auth.json", "cloud-ledger", ".config/oxc"];

/// APIs that open a channel or read a file the panel must not use.
const FORBIDDEN_APIS: &[&str] = &["XMLHttpRequest", "fetch(", "WebSocket", "FileView", "Socket"];

fn panel_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("omarchy-panel")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Every file under the panel folder as (path relative to the folder, text).
fn panel_files() -> Vec<(String, String)> {
    let root = panel_dir();
    let mut paths = Vec::new();
    collect(&root, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(&root)
                .expect("path is under the panel folder")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name} as UTF-8 text: {e}"));
            (name, text)
        })
        .collect()
}

fn is_source(name: &str) -> bool {
    name.ends_with(".qml") || name.ends_with(".js")
}

fn file_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn declares_process(text: &str) -> bool {
    text.contains("Process {") || text.contains("Process{")
}

#[test]
fn panel_reads_only_oxa_json() {
    let mut problems = Vec::new();
    for (name, text) in panel_files() {
        let lower = text.to_lowercase();
        for word in FORBIDDEN_TEXT {
            if lower.contains(*word) {
                problems.push(format!("{name}: contains \"{word}\""));
            }
        }
        for api in FORBIDDEN_APIS {
            if text.contains(*api) {
                problems.push(format!("{name}: uses {api}"));
            }
        }
        if text.contains("sh -c") || text.contains("\"sh\"") || text.contains("\"bash\"") {
            problems.push(format!("{name}: runs a shell"));
        }
        if !is_source(&name) {
            continue;
        }
        if declares_process(&text) && !PROCESS_FILES.contains(&file_name(&name)) {
            problems.push(format!("{name}: declares a Process outside Feeds.qml"));
        }
        for line in text.lines() {
            // A url may be opened only when it is the lead page: the status url or the leadUrl setting.
            if line.contains("openUrlExternally") && !line.contains("serve") && !line.contains("leadUrl") {
                problems.push(format!(
                    "{name}: openUrlExternally on a url that is not the lead page: {}",
                    line.trim()
                ));
            }
            // Every program the panel starts is the oxa control binary, given as an argument list.
            if (line.contains("command:") || line.contains(".command ="))
                && !line.contains("controlExecutable")
            {
                problems.push(format!(
                    "{name}: a command that does not start with the oxa binary: {}",
                    line.trim()
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "the panel reads outside oxa JSON:\n{}",
        problems.join("\n")
    );
}

/// The first `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` literal on the line, if there is one.
fn hex_colour(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    for (start, &byte) in bytes.iter().enumerate() {
        if byte != b'#' {
            continue;
        }
        let digits = bytes[start + 1..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        let ends_token = !matches!(bytes.get(start + 1 + digits), Some(b) if b.is_ascii_alphanumeric());
        if matches!(digits, 3 | 4 | 6 | 8) && ends_token {
            return Some(line[start..start + 1 + digits].to_string());
        }
    }
    None
}

#[test]
fn panel_has_no_hex_colours() {
    let mut problems = Vec::new();
    for (name, text) in panel_files() {
        if !is_source(&name) {
            continue;
        }
        for (number, line) in text.lines().enumerate() {
            if let Some(hex) = hex_colour(line) {
                problems.push(format!("{name}:{}: {hex}", number + 1));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "hex colours in the panel (use the Commons.Color tokens):\n{}",
        problems.join("\n")
    );
}

/// The literal keys of every `.t("…")` call in the source. A literal that ends in a dot is the
/// prefix of a key built at run time, so it only has to match some key.
fn translated_keys(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(".t(\"") {
        let after = &rest[at + 4..];
        let Some(end) = after.find('"') else {
            break;
        };
        let key = &after[..end];
        if !key.is_empty() {
            keys.push(key.to_string());
        }
        rest = &after[end + 1..];
    }
    keys
}

#[test]
fn panel_strings_exist() {
    let path = panel_dir().join("i18n").join("en.json");
    let text = fs::read_to_string(&path).expect("read i18n/en.json");
    let table: Value = serde_json::from_str(&text).expect("en.json is valid JSON");
    let keys: BTreeSet<String> = table
        .as_object()
        .expect("en.json is an object")
        .keys()
        .cloned()
        .collect();

    let mut problems = Vec::new();
    for (name, text) in panel_files() {
        if !is_source(&name) {
            continue;
        }
        for key in translated_keys(&text) {
            let found = if key.ends_with('.') {
                keys.iter().any(|k| k.starts_with(key.as_str()))
            } else {
                keys.contains(&key)
            };
            if !found {
                problems.push(format!("{name}: no key \"{key}\" in i18n/en.json"));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "translation keys missing from i18n/en.json:\n{}",
        problems.join("\n")
    );
}
