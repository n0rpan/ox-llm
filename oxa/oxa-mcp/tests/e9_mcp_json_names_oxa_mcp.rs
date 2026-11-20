//! E9 acceptance tests: mcp.json, names, oxa mcp.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_mcp::*;
use oxa_proto::{codes, Exposure, OxaError, Value};
use serde_json::json;
use std::path::Path;

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn server_names(config: &McpConfig) -> Vec<&str> {
    config.servers.iter().map(|s| s.name.as_str()).collect()
}

fn find<'a>(config: &'a McpConfig, name: &str) -> &'a ServerConfig {
    config
        .servers
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("server {name} missing"))
}

/// merge rules of mcp.md
#[test]
fn user_project_replace_and_partial_override() {
    let env = TestEnv::new();
    let user = env.write(
        "home/.oxa/agent/mcp.json",
        r#"{
  "autoEnableCodemode": true,
  "mcpServers": {
    "alpha": { "command": "user-alpha", "args": ["--u"], "description": "user alpha" },
    "beta": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${TOK}" }, "exposure": "deferred" },
    "gamma": { "command": "gamma" }
  }
}"#,
    );
    let project = env.write(
        "work/.oxa/mcp.json",
        r#"{
  "autoEnableCodemode": false,
  "mcpServers": {
    "alpha": { "command": "project-alpha" },
    "beta": { "enabled": false, "exposure": "direct", "toolExposure": { "search_*": "hidden" } },
    "gamma": { "enabled": false, "timeout": 5 },
    "delta": { "enabled": false }
  }
}"#,
    );

    let config = load_config(&user, Some(&project));
    assert_eq!(server_names(&config), ["alpha", "beta", "gamma"]);
    assert_eq!(config.auto_enable_codemode, Some(false));

    // A full project entry replaces the user entry of the same name.
    let alpha = find(&config, "alpha");
    assert_eq!(alpha.defined_in, ConfigScope::Project);
    assert!(!alpha.project_override);
    assert!(matches!(&alpha.kind, ServerKind::Stdio(s) if s.command == "project-alpha" && s.args.is_empty()));
    assert_eq!(alpha.description, None);

    // A project entry without command/url/type keeps the rest of the user entry.
    let beta = find(&config, "beta");
    assert_eq!(beta.defined_in, ConfigScope::User);
    assert!(beta.project_override);
    assert!(!beta.enabled);
    assert_eq!(beta.exposure, Exposure::Direct);
    assert_eq!(
        beta.tool_exposure,
        vec![("search_*".to_string(), Exposure::Hidden)]
    );
    match &beta.kind {
        ServerKind::Http(h) => {
            assert_eq!(h.url, "https://example.com/mcp");
            assert_eq!(
                h.headers,
                vec![("Authorization".to_string(), "Bearer ${TOK}".to_string())]
            );
        }
        other => panic!("beta should stay HTTP, got {other:?}"),
    }

    // An override may set only enabled, exposure, toolExposure: `timeout` is a diagnostic and
    // the user entry is unchanged. An override without a server to override is a diagnostic.
    let gamma = find(&config, "gamma");
    assert!(gamma.enabled && !gamma.project_override);
    assert!(config
        .diagnostics
        .iter()
        .any(|d| d.contains("gamma") && d.contains("can only set")));
    assert!(config
        .diagnostics
        .iter()
        .any(|d| d.contains("delta") && d.contains("needs")));
    assert!(!config.servers.iter().any(|s| s.name == "delta"));

    // Without a project file the user file alone is used.
    let user_only = load_config(&user, None);
    assert_eq!(server_names(&user_only), ["alpha", "beta", "gamma"]);
    assert!(find(&user_only, "alpha").description.is_some());
    assert_eq!(user_only.auto_enable_codemode, Some(true));
}

/// `!command` resolved values
#[test]
fn bang_command_values() {
    let lookup = |name: &str| match name {
        "TOK" => Some("abc".to_string()),
        _ => None,
    };
    let seen = std::cell::RefCell::new(Vec::<String>::new());
    let run = |cmd: &str| -> Result<String, OxaError> {
        seen.borrow_mut().push(cmd.to_string());
        if cmd == "fail" {
            return Err(OxaError::new(codes::MCP, "boom"));
        }
        Ok(format!("  Bearer token-for-{cmd}\n"))
    };

    // Whole value starting with `!`: the trimmed stdout of the command.
    let value = expand_value("!echo Bearer $(gh auth token)", &lookup, &run).unwrap();
    assert_eq!(value, "Bearer token-for-echo Bearer $(gh auth token)");
    assert_eq!(seen.borrow().last().unwrap(), "echo Bearer $(gh auth token)");

    // `!` inside a value is literal; `${VAR}` expands anywhere, unset = empty.
    assert_eq!(
        expand_value("Bearer ${TOK}!", &lookup, &run).unwrap(),
        "Bearer abc!"
    );
    assert_eq!(expand_value("a${NOPE}b", &lookup, &run).unwrap(), "ab");
    assert_eq!(expand_value("x !y", &lookup, &run).unwrap(), "x !y");
    assert_eq!(expand_value("${UNCLOSED", &lookup, &run).unwrap(), "${UNCLOSED");
    // A command failure is an error, not an empty value.
    assert!(expand_value("!fail", &lookup, &run).is_err());

    // Values are kept raw in the parsed configuration (expanded at connect time).
    let entry =
        json!({ "url": "https://x.example/mcp", "headers": { "Authorization": "!printf 'Bearer t'" } });
    let server = parse_server("docs", &entry, ConfigScope::User).unwrap();
    match server.kind {
        ServerKind::Http(h) => assert_eq!(h.headers[0].1, "!printf 'Bearer t'"),
        other => panic!("expected HTTP, got {other:?}"),
    }
}

/// mcp__server__tool, invalid characters
#[test]
fn name_mangling() {
    assert_eq!(mangle("dev-radius", "get.items"), "mcp__dev_radius__get_items");
    assert_eq!(mangle("docs", "search"), "mcp__docs__search");
    assert!(valid_server_name("dev-radius_2"));
    assert!(!valid_server_name("dev radius"));
    assert!(!valid_server_name("dev.radius"));
    assert!(!valid_server_name(""));
    assert!(same_server("dev-radius", "dev_radius"));
    assert!(!same_server("dev", "devs"));
    assert_eq!(
        tool_names("docs", &["search".to_string(), "read".to_string()]),
        ["mcp__docs__search", "mcp__docs__read"]
    );
    assert_eq!(split_full_name("mcp__docs__search"), Some(("docs", "search")));
    assert_eq!(split_full_name("docs__search"), None);
    assert_eq!(split_full_name("mcp__docs"), None);
    assert!(pattern_matches("search_*", "search_code"));
    assert!(!pattern_matches("search_*", "get_search"));
    assert!(pattern_matches("get_*_items", "get_my_items"));
    assert!(!pattern_matches("get", "get_items"));
    assert_eq!(RESOURCE_TOOLS.len(), 3);
}

/// long or colliding names
#[test]
fn collision_hash() {
    use sha2::{Digest, Sha256};
    let hash8 = |s: &str| -> String {
        Sha256::digest(s.as_bytes())[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };

    // Colliding mangled names: every colliding tool gets the suffix of its own original name.
    let names = tool_names("s", &["a-b".to_string(), "a_b".to_string(), "other".to_string()]);
    assert_eq!(names[0], format!("mcp__s__a_b_{}", hash8("a-b")));
    assert_eq!(names[1], format!("mcp__s__a_b_{}", hash8("a_b")));
    assert_eq!(names[2], "mcp__s__other");
    assert_eq!(names[0].len(), names[1].len());

    // Deterministic.
    assert_eq!(
        tool_names("s", &["a-b".to_string(), "a_b".to_string()]),
        names[..2]
    );

    // Long names are capped at the provider limit of 64 characters, with the hash suffix.
    let long = "t".repeat(90);
    let names = tool_names("srv", std::slice::from_ref(&long));
    assert_eq!(names[0].len(), 64);
    assert!(names[0].starts_with("mcp__srv__ttt"));
    assert!(names[0].ends_with(&format!("_{}", hash8(&long))));

    // Two long tools that share a 64-character head stay distinct.
    let a = format!("{}a", "q".repeat(80));
    let b = format!("{}b", "q".repeat(80));
    let both = tool_names("srv", &[a, b]);
    assert_ne!(both[0], both[1]);

    // Server names that differ only in `-` / `_` are one server: the second is a diagnostic.
    let env = TestEnv::new();
    let file = env.write(
        "home/.oxa/agent/mcp.json",
        r#"{"mcpServers": {"dev-radius": {"command": "a"}, "dev_radius": {"command": "b"}}}"#,
    );
    let config = load_config(&file, None);
    assert_eq!(server_names(&config), ["dev-radius"]);
    assert!(config.diagnostics.iter().any(|d| d.contains("conflicts")));
}

/// file edit
#[test]
fn add_keeps_unrelated_content() {
    let env = TestEnv::new();
    let file = env.write(
        "home/.oxa/agent/mcp.json",
        "{\n  \"zeta\": 1,\n  \"mcpServers\": {\n    \"keep\": { \"command\": \"keep-me\" },\n    \"change\": { \"command\": \"old\" }\n  },\n  \"alpha\": [1, 2]\n}\n",
    );

    add_server(&file, "change", &json!({ "command": "new", "args": ["--x"] })).unwrap();
    add_server(&file, "fresh", &json!({ "url": "https://h.example/mcp" })).unwrap();

    let doc = read_json(&file);
    assert_eq!(doc["zeta"], json!(1));
    assert_eq!(doc["alpha"], json!([1, 2]));
    assert_eq!(doc["mcpServers"]["keep"], json!({ "command": "keep-me" }));
    assert_eq!(
        doc["mcpServers"]["change"],
        json!({ "command": "new", "args": ["--x"] })
    );
    assert_eq!(
        doc["mcpServers"]["fresh"],
        json!({ "url": "https://h.example/mcp" })
    );

    // Order of top-level keys and of servers is kept; the replaced server keeps its place.
    let top: Vec<&str> = doc.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(top, ["zeta", "mcpServers", "alpha"]);
    let servers: Vec<&str> = doc["mcpServers"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(servers, ["keep", "change", "fresh"]);

    // A missing file is created with its parent directory.
    let fresh_file = env.work().join(".oxa/mcp.json");
    add_server(&fresh_file, "p", &json!({ "command": "p" })).unwrap();
    assert_eq!(
        read_json(&fresh_file),
        json!({ "mcpServers": { "p": { "command": "p" } } })
    );

    // Invalid JSON is never overwritten.
    let broken = env.write("work/broken.json", "{ not json");
    let err = add_server(&broken, "x", &json!({})).unwrap_err();
    assert_eq!(err.code, codes::INVALID);
    assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{ not json");
}

/// file edit
#[test]
fn remove_keeps_unrelated_content() {
    let env = TestEnv::new();
    let file = env.write(
        "home/.oxa/agent/mcp.json",
        r#"{"mcpServers": {"a": {"command": "a"}, "b": {"command": "b"}, "c": {"enabled": false}}, "keep": {"x": true}}"#,
    );
    assert!(remove_server(&file, "b").unwrap());
    let doc = read_json(&file);
    assert_eq!(doc["keep"], json!({ "x": true }));
    assert_eq!(
        doc["mcpServers"],
        json!({ "a": { "command": "a" }, "c": { "enabled": false } })
    );
    let servers: Vec<&str> = doc["mcpServers"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(servers, ["a", "c"]);

    // Not there: Ok(false) and the file is not touched.
    let before = std::fs::read_to_string(&file).unwrap();
    assert!(!remove_server(&file, "missing").unwrap());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    assert!(!remove_server(&env.work().join("none.json"), "x").unwrap());

    // An override is set on a bare entry when the file lacks the server.
    set_server_field(&file, "a", "enabled", &json!(false)).unwrap();
    set_server_field(&file, "project-only", "enabled", &json!(false)).unwrap();
    let doc = read_json(&file);
    assert_eq!(
        doc["mcpServers"]["a"],
        json!({ "command": "a", "enabled": false })
    );
    assert_eq!(doc["mcpServers"]["project-only"], json!({ "enabled": false }));
    assert_eq!(doc["keep"], json!({ "x": true }));
}

/// invalid entry -> 1
#[test]
fn list_json_and_exit_code() {
    // `oxa mcp list` exits 1 exactly when the configuration has diagnostics (an invalid entry or
    // an unreadable file); a clean configuration has none.
    let env = TestEnv::new();
    let clean = env.write(
        "home/.oxa/agent/mcp.json",
        r#"{"mcpServers": {"ok": {"command": "ok", "exposure": "direct"}}}"#,
    );
    let config = load_config(&clean, None);
    assert!(config.diagnostics.is_empty());
    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.servers[0].exposure, Exposure::Direct);

    let bad = env.write(
        "work/bad.json",
        r#"{"mcpServers": {"ok": {"command": "ok"}, "broken": {"url": "ftp://x"}}}"#,
    );
    let config = load_config(&bad, None);
    assert_eq!(config.diagnostics.len(), 1, "{:?}", config.diagnostics);
    assert!(!config.diagnostics.is_empty());
}

/// other servers still load
#[test]
fn invalid_entry_diagnostic() {
    let env = TestEnv::new();
    let file = env.write(
        "home/.oxa/agent/mcp.json",
        r#"{"mcpServers": {
  "good": { "command": "g" },
  "legacy": { "type": "sse", "url": "https://x.example/sse" },
  "bad name": { "command": "b" },
  "noTransport": { "description": "neither command nor url" },
  "badExposure": { "command": "c", "exposure": "everywhere" },
  "web": { "url": "https://x.example/mcp", "oauth": { "callbackUrl": "https://evil.example/cb" } },
  "other": { "command": "o", "timeout": 30, "toolExposure": { "t": "hidden" } }
}}"#,
    );
    let config = load_config(&file, None);
    assert_eq!(server_names(&config), ["good", "other"]);
    assert_eq!(config.diagnostics.len(), 5, "{:#?}", config.diagnostics);
    assert!(config
        .diagnostics
        .iter()
        .any(|d| d.contains("legacy") && d.contains("SSE")));
    assert!(config.diagnostics.iter().any(|d| d.contains("bad name")));
    assert!(config.diagnostics.iter().any(|d| d.contains("noTransport")));
    assert!(config.diagnostics.iter().any(|d| d.contains("badExposure")));
    assert!(config
        .diagnostics
        .iter()
        .any(|d| d.contains("web") && d.contains("callbackUrl")));
    assert_eq!(find(&config, "other").timeout_s, 30);
    assert_eq!(
        find(&config, "other").tool_exposure,
        vec![("t".to_string(), Exposure::Hidden)]
    );

    // Unparsable JSON is one diagnostic and an empty file.
    let broken = env.write("work/broken.json", "{ \"mcpServers\": ");
    let config = load_config(&broken, None);
    assert!(config.servers.is_empty());
    assert_eq!(config.diagnostics.len(), 1);
}

/// trust gate
#[test]
fn untrusted_project_file_skipped() {
    let env = TestEnv::new();
    let user = env.write(
        "home/.oxa/agent/mcp.json",
        r#"{"mcpServers": {"u": {"command": "u"}}}"#,
    );
    let project = env.write(
        "work/.oxa/mcp.json",
        r#"{"mcpServers": {"p": {"command": "p"}, "u": {"enabled": false}}}"#,
    );
    // Not trusted: the project file is not read at all.
    let untrusted = load_config(&user, None);
    assert_eq!(server_names(&untrusted), ["u"]);
    assert!(find(&untrusted, "u").enabled);
    assert!(untrusted.diagnostics.is_empty());

    // Trusted: the project entry and override apply.
    let trusted = load_config(&user, Some(&project));
    assert_eq!(server_names(&trusted), ["p", "u"]);
    assert!(!find(&trusted, "u").enabled);

    // The project file path of a working directory is `<cwd>/.oxa/mcp.json`.
    assert_eq!(
        project_config_path(env.work().as_path()),
        env.work().join(".oxa").join("mcp.json")
    );
    assert_eq!(
        user_config_path(&env.agent_dir()),
        env.agent_dir().join("mcp.json")
    );
}

/// Extra coverage: `oxa mcp add` option validation builds exactly what the loader accepts.
#[test]
fn add_options_build_valid_entries() {
    let stdio = AddOptions {
        command: vec!["npx".into(), "-y".into(), "pkg".into()],
        env: vec![("API_KEY".into(), "${TOOLS_KEY}".into())],
        cwd: Some("/srv".into()),
        exposure: Some("codemode-deferred".into()),
        description: Some("files".into()),
        ..Default::default()
    };
    let entry = entry_from_add_options("fs", &stdio).unwrap();
    assert_eq!(
        entry,
        json!({
            "command": "npx",
            "args": ["-y", "pkg"],
            "env": { "API_KEY": "${TOOLS_KEY}" },
            "cwd": "/srv",
            "exposure": "codemode",
            "description": "files"
        })
    );

    let http = AddOptions {
        url: Some("https://docs.example/mcp".into()),
        bearer_token_env_var: Some("DOCS_TOKEN".into()),
        headers: vec![("X-Team".into(), "a".into())],
        oauth_callback_port: Some(8765),
        oauth_client_registration: Some("dcr".into()),
        ..Default::default()
    };
    let entry = entry_from_add_options("docs", &http).unwrap();
    assert_eq!(entry["headers"]["Authorization"], json!("Bearer ${DOCS_TOKEN}"));
    assert_eq!(
        entry["oauth"],
        json!({ "callbackPort": 8765, "clientRegistration": "dcr" })
    );
    assert!(parse_server("docs", &entry, ConfigScope::User).is_ok());

    // Usage errors.
    let both = AddOptions {
        command: vec!["x".into()],
        url: Some("https://x.example".into()),
        ..Default::default()
    };
    assert_eq!(entry_from_add_options("x", &both).unwrap_err().code, codes::USAGE);
    assert_eq!(
        entry_from_add_options("x", &AddOptions::default())
            .unwrap_err()
            .code,
        codes::USAGE
    );
    let bad_name = AddOptions {
        command: vec!["x".into()],
        ..Default::default()
    };
    assert_eq!(
        entry_from_add_options("a b", &bad_name).unwrap_err().code,
        codes::USAGE
    );
    let headers_on_stdio = AddOptions {
        command: vec!["x".into()],
        headers: vec![("A".into(), "b".into())],
        ..Default::default()
    };
    assert_eq!(
        entry_from_add_options("x", &headers_on_stdio).unwrap_err().code,
        codes::USAGE
    );
}

/// Extra coverage: the OAuth settings of an HTTP server are parsed and checked.
#[test]
fn oauth_settings_parse() {
    let entry = json!({
        "url": "https://mcp.example.com/mcp",
        "oauth": {
            "clientId": "my-client",
            "clientSecret": "${EXAMPLE_SECRET}",
            "callbackPort": 8765,
            "scope": "read write",
            "clientRegistration": "dcr"
        }
    });
    let server = parse_server("example", &entry, ConfigScope::User).unwrap();
    let ServerKind::Http(http) = server.kind else {
        panic!("expected HTTP")
    };
    let oauth = http.oauth.unwrap();
    assert_eq!(oauth.client_id.as_deref(), Some("my-client"));
    assert_eq!(oauth.callback_port, Some(8765));
    assert_eq!(oauth.client_registration, ClientRegistration::Dcr);

    let loopback = json!({ "url": "https://x.example/mcp", "oauth": { "callbackUrl": "http://localhost:9000/callback" } });
    assert!(parse_server("x", &loopback, ConfigScope::User).is_ok());
    let mismatch = json!({ "url": "https://x.example/mcp", "oauth": { "callbackUrl": "http://localhost:9000/cb", "callbackPort": 9001 } });
    assert!(parse_server("x", &mismatch, ConfigScope::User).is_err());
    let metadata_http = json!({ "url": "https://x.example/mcp", "oauth": { "authServerMetadataUrl": "http://auth.example/x" } });
    assert!(parse_server("x", &metadata_http, ConfigScope::User).is_err());
    let metadata_local = json!({ "url": "https://x.example/mcp", "oauth": { "authServerMetadataUrl": "http://127.0.0.1:9/x" } });
    assert!(parse_server("x", &metadata_local, ConfigScope::User).is_ok());
    let sse = json!({ "type": "sse", "url": "https://x.example/sse" });
    assert!(parse_server("x", &sse, ConfigScope::User)
        .unwrap_err()
        .contains("/mcp"));
}

/// Extra coverage: effective exposure follows exact names, then the first matching pattern.
#[test]
fn effective_exposure_order() {
    let entry = json!({
        "command": "x",
        "exposure": "deferred",
        "toolExposure": { "get_*": "codemode", "get_items": "direct", "delete_*": "hidden" }
    });
    let server = parse_server("s", &entry, ConfigScope::User).unwrap();
    assert_eq!(effective_exposure(&server, "get_items"), Exposure::Direct);
    assert_eq!(effective_exposure(&server, "get_other"), Exposure::Codemode);
    assert_eq!(effective_exposure(&server, "delete_all"), Exposure::Hidden);
    assert_eq!(effective_exposure(&server, "list"), Exposure::Deferred);
}
