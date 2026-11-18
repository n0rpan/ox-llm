//! B6 acceptance tests: Model resolution, env table, dirs.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{
    dirs_from_env, resolve_model, resolve_scoped, CloudApi, Dirs, Endpoint, Env, ModelRef, ModelsConfig,
    ENV_TABLE,
};
use oxa_fake::TestEnv;
use oxa_proto::{codes, ThinkingLevel};

fn mr(endpoint: &str, model: &str) -> ModelRef {
    ModelRef {
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        thinking: None,
    }
}

fn catalog() -> Vec<ModelRef> {
    vec![
        mr("oxe", "flash-20b"),
        mr("oxe", "qwen3-coder-30b"),
        mr("laptop", "qwen3-coder-30b"),
        mr("oxe", "gemma-4-12b-it"),
        mr("oxe", "flash-20b-long"),
    ]
}

/// resolution order
#[test]
fn exact_fuzzy_glob_patterns() {
    let models = catalog();
    // exact id beats a substring match that would also hit `flash-20b-long`
    assert_eq!(
        resolve_model("flash-20b", None, &models).unwrap(),
        mr("oxe", "flash-20b")
    );
    // endpoint/model is exact on the endpoint's own id
    assert_eq!(
        resolve_model("laptop/qwen3-coder-30b", None, &models).unwrap(),
        mr("laptop", "qwen3-coder-30b")
    );
    // case-insensitive substring (fuzzy)
    assert_eq!(
        resolve_model("GEMMA", None, &models).unwrap(),
        mr("oxe", "gemma-4-12b-it")
    );
    // glob, when no exact or fuzzy hit exists
    assert_eq!(
        resolve_model("gem*-it", None, &models).unwrap(),
        mr("oxe", "gemma-4-12b-it")
    );
    assert_eq!(
        resolve_model("flash-20?", None, &models).unwrap(),
        mr("oxe", "flash-20b")
    );
    // provider constraint restricts the pool
    assert_eq!(
        resolve_model("qwen3", Some("laptop"), &models).unwrap(),
        mr("laptop", "qwen3-coder-30b")
    );
    let err = resolve_model("nothing-like-this", None, &models).unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
    assert!(
        err.hint.unwrap().contains("oxe/flash-20b"),
        "not_found lists the candidates"
    );
}

/// `model:high`
#[test]
fn thinking_suffix() {
    let models = catalog();
    let r = resolve_model("flash-20b:high", None, &models).unwrap();
    assert_eq!(r.model, "flash-20b");
    assert_eq!(r.thinking, Some(ThinkingLevel::High));
    assert_eq!(resolve_model("flash-20b", None, &models).unwrap().thinking, None);
    // an invalid suffix stays part of the id and does not resolve a different model
    assert_eq!(
        resolve_model("flash-20b:banana", None, &models).unwrap_err().code,
        codes::NOT_FOUND
    );
    // the thinking level combines with an endpoint prefix
    let r = resolve_model("laptop/qwen3:off", None, &models).unwrap();
    assert_eq!(
        (r.endpoint.as_str(), r.thinking),
        ("laptop", Some(ThinkingLevel::Off))
    );
}

/// --models
#[test]
fn scoped_list_order_no_duplicates() {
    let models = catalog();
    let patterns: Vec<String> = ["gemma", "flash-20b:low", "qwen3", "gemma", "laptop/*", "nope-*"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (scoped, diags) = resolve_scoped(&patterns, &models);
    let got: Vec<(String, String, Option<ThinkingLevel>)> = scoped
        .iter()
        .map(|m| (m.endpoint.clone(), m.model.clone(), m.thinking))
        .collect();
    assert_eq!(
        got,
        vec![
            ("oxe".to_string(), "gemma-4-12b-it".to_string(), None),
            (
                "oxe".to_string(),
                "flash-20b".to_string(),
                Some(ThinkingLevel::Low)
            ),
            ("oxe".to_string(), "qwen3-coder-30b".to_string(), None),
            ("laptop".to_string(), "qwen3-coder-30b".to_string(), None),
        ],
        "pattern order, no duplicates"
    );
    assert_eq!(
        diags.len(),
        1,
        "only the pattern that matches nothing is reported: {diags:?}"
    );
    assert!(diags[0].message.contains("nope-*"));
}

/// unsupported_in_oxa + hint
#[test]
fn non_engine_provider_clear_error() {
    let models = catalog();
    let err = resolve_model("claude-opus", Some("anthropic"), &models).unwrap_err();
    assert_eq!(err.code, codes::UNSUPPORTED_IN_OXA);
    assert!(err.hint.as_deref().unwrap_or("").contains("local engine"));
    // the same refusal when the provider is written as a prefix
    let err = resolve_model("openai/gpt-6", None, &models).unwrap_err();
    assert_eq!(err.code, codes::UNSUPPORTED_IN_OXA);
    // a models.json entry for a cloud provider is a diagnostic, not a failure
    let t = TestEnv::new();
    let dirs = Dirs::under(t.root());
    t.write(
        "home/.oxa/agent/models.json",
        r#"{"providers": {"anthropic": {"baseUrl": "http://127.0.0.1:9", "api": "anthropic-messages"}}}"#,
    );
    let (cfg, diags) = ModelsConfig::load(&dirs, &Env::from_pairs(&[]));
    assert_eq!(cfg.endpoints.len(), 1, "falls back to the default endpoint");
    assert!(diags.iter().any(|d| d.message.contains("unsupported_in_oxa")));
}

/// every variable of environment-variables.md has an OXA_ row
#[test]
fn oxa_env_table_complete() {
    // Every variable named in pi's environment-variables.md (PI_* names plus the standard ones).
    let doc_vars = [
        "AI_AGENT",
        "PI_CODING_AGENT",
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
        "PI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "PI_PACKAGE_DIR",
        "PI_OFFLINE",
        "PI_SKIP_VERSION_CHECK",
        "PI_SELF_UPDATE_COMMAND",
        "PI_TELEMETRY",
        "PI_CODEMODE_MAX_WORKERS",
        "PI_CACHE_RETENTION",
        "PI_SHARE_VIEWER_URL",
        "PI_RADIUS_GATEWAY",
        "PI_CLEAR_ON_SHRINK",
        "PI_CLIPBOARD_OSC52",
        "VISUAL",
        "EDITOR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
    ];
    for var in doc_vars {
        let row = ENV_TABLE
            .iter()
            .find(|r| r.pi_name == Some(var) || (r.pi_name.is_none() && r.name == var));
        let row = row.unwrap_or_else(|| panic!("no row for {var}"));
        if var.starts_with("PI_") {
            assert!(row.name.starts_with("OXA_"), "{var} maps to {}", row.name);
        }
        assert!(!row.description.is_empty());
    }
    // names are unique
    let mut names: Vec<&str> = ENV_TABLE.iter().map(|r| r.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), ENV_TABLE.len());
    // the verdict O variables are marked unsupported
    let o = ENV_TABLE.iter().find(|r| r.name == "OXA_TELEMETRY").unwrap();
    assert!(!o.supported);
    let a = ENV_TABLE.iter().find(|r| r.name == "OXA_OFFLINE").unwrap();
    assert!(a.supported);
}

/// OXA_AGENT_DIR
#[test]
fn agent_dir_override_and_tilde() {
    let t = TestEnv::new();
    let home = t.home().to_string_lossy().into_owned();
    // override with an absolute path
    let abs = t.root().join("elsewhere/agent").to_string_lossy().into_owned();
    let d = dirs_from_env(
        &Env::from_pairs(&[("HOME", home.as_str()), ("OXA_AGENT_DIR", abs.as_str())]),
        None,
    )
    .unwrap();
    assert_eq!(d.agent_dir, t.root().join("elsewhere/agent"));
    assert_eq!(d.sessions_dir, d.agent_dir.join("sessions"));
    assert_eq!(d.tmp_dir, d.agent_dir.join("tmp"));
    // tilde expansion
    let d = dirs_from_env(
        &Env::from_pairs(&[("HOME", home.as_str()), ("OXA_AGENT_DIR", "~/state/oxa")]),
        None,
    )
    .unwrap();
    assert_eq!(d.agent_dir, t.home().join("state/oxa"));
    // default layout
    let d = dirs_from_env(&Env::from_pairs(&[("HOME", home.as_str())]), None).unwrap();
    assert_eq!(d.agent_dir, t.home().join(".oxa/agent"));
    // session dir: flag/setting override beats OXA_SESSION_DIR, which beats the default
    let flag = t.root().join("flag-sessions");
    let d = dirs_from_env(
        &Env::from_pairs(&[("HOME", home.as_str()), ("OXA_SESSION_DIR", "~/s")]),
        Some(flag.as_path()),
    )
    .unwrap();
    assert_eq!(d.sessions_dir, flag);
    let d = dirs_from_env(
        &Env::from_pairs(&[("HOME", home.as_str()), ("OXA_SESSION_DIR", "~/s")]),
        None,
    )
    .unwrap();
    assert_eq!(d.sessions_dir, t.home().join("s"));
    // no HOME is an invalid error with a hint
    let err = dirs_from_env(&Env::from_pairs(&[]), None).unwrap_err();
    assert_eq!(err.code, codes::INVALID);
    assert!(err.hint.is_some());
}

/// ~/.pi present, not used
#[test]
fn pi_dirs_never_read() {
    let t = TestEnv::new();
    t.write("home/.pi/agent/settings.json", "{}");
    t.write(
        "home/.pi/agent/models.json",
        r#"{"providers": {"oxe": {"baseUrl": "http://10.0.0.9:1"}}}"#,
    );
    let home = t.home().to_string_lossy().into_owned();
    let d = dirs_from_env(&Env::from_pairs(&[("HOME", home.as_str())]), None).unwrap();
    assert!(!d.agent_dir.to_string_lossy().contains(".pi"));
    assert_eq!(d.agent_dir, t.home().join(".oxa/agent"));
    // the pi models.json is not read: the default endpoint is used
    let (cfg, diags) = ModelsConfig::load(&d, &Env::from_pairs(&[]));
    assert!(diags.is_empty());
    assert_eq!(cfg.endpoints[0].base_url, "http://127.0.0.1:8092");
}

/// OXA_ENGINE_URL or 127.0.0.1:8092 (the controller)
#[test]
fn default_endpoint() {
    let t = TestEnv::new();
    let dirs = Dirs::under(t.root());
    let (cfg, diags) = ModelsConfig::load(&dirs, &Env::from_pairs(&[]));
    assert!(diags.is_empty());
    assert_eq!(cfg.default, "oxe");
    assert_eq!(
        cfg.endpoints,
        vec![Endpoint {
            name: "oxe".into(),
            base_url: "http://127.0.0.1:8092".into(),
            api_key: None
        }]
    );
    let (cfg, _) = ModelsConfig::load(
        &dirs,
        &Env::from_pairs(&[("OXA_ENGINE_URL", "http://192.168.1.20:8080/")]),
    );
    assert_eq!(cfg.endpoints[0].base_url, "http://192.168.1.20:8080");
}

#[test]
fn models_json_endpoints_keys_and_default() {
    let t = TestEnv::new();
    let dirs = Dirs::under(t.root());
    t.write(
        "home/.oxa/agent/models.json",
        r#"{"default": "laptop", "providers": {
            "laptop": {"baseUrl": "http://192.168.1.7:8080/", "api": "openai-completions", "apiKey": "$LAPTOP_KEY"},
            "lan": {"baseUrl": "http://10.0.0.2:8080", "api": "openai-completions", "apiKey": "literal-key"},
            "bad": {"baseUrl": "https://example.invalid", "api": "openai-completions"},
            "cmd": {"baseUrl": "http://10.0.0.3:8080", "api": "openai-completions", "apiKey": "!pass show x"}
        }}"#,
    );
    let (cfg, diags) = ModelsConfig::load(&dirs, &Env::from_pairs(&[("LAPTOP_KEY", "s3cret")]));
    assert_eq!(cfg.default, "laptop");
    let names: Vec<&str> = cfg.endpoints.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["laptop", "lan", "cmd"], "https entries are skipped");
    assert_eq!(cfg.endpoints[0].base_url, "http://192.168.1.7:8080");
    assert_eq!(cfg.endpoints[0].api_key.as_deref(), Some("s3cret"));
    assert_eq!(cfg.endpoints[1].api_key.as_deref(), Some("literal-key"));
    assert_eq!(cfg.endpoints[2].api_key, None, "commands are not run");
    assert!(diags.iter().any(|d| d.message.contains("https")));
    assert!(diags.iter().any(|d| d.message.contains("apiKey commands")));
}

#[test]
fn models_json_accepts_https_cloud_protocols_alongside_the_engine() {
    let t = TestEnv::new();
    let dirs = Dirs::under(t.root());
    t.write(
        "home/.oxa/agent/models.json",
        r#"{"default":"laptop","providers":{
            "laptop":{"api":"openai-completions","baseUrl":"http://192.168.1.7:8080"},
            "chatbox":{"api":"openai-chat","baseUrl":"https://chat.example.invalid/v1",
                "key":"chat-key-ref","pool":"api-monthly",
                "models":[{"id":"chat-model","context":32768,"price":{"input":1.25,"output":4}}]},
            "responsebox":{"api":"openai-responses","baseUrl":"https://responses.example.invalid/v1",
                "key":"response-key-ref","pool":"api-monthly"},
            "messagebox":{"api":"anthropic-messages","baseUrl":"https://messages.example.invalid/v1",
                "key":"message-key-ref","pool":"api-monthly",
                "models":[{"id":"message-model","context":32768}]}
        }}"#,
    );
    let (cfg, diags) = ModelsConfig::load(&dirs, &Env::from_pairs(&[]));
    assert!(
        diags.is_empty(),
        "valid cloud entries should be recognized: {diags:?}"
    );
    assert_eq!(cfg.default, "laptop");
    assert_eq!(cfg.endpoints.len(), 1);
    assert_eq!(cfg.endpoints[0].base_url, "http://192.168.1.7:8080");
    assert_eq!(cfg.cloud_providers.len(), 3);
    let chat = &cfg.cloud_providers["chatbox"];
    assert_eq!(chat.api, CloudApi::OpenAiChat);
    assert_eq!(chat.base_url, "https://chat.example.invalid/v1");
    assert_eq!(chat.key, "chat-key-ref");
    assert_eq!(chat.pool, "api-monthly");
    assert_eq!(chat.models[0].id, "chat-model");
    assert_eq!(chat.models[0].context, 32768);
    assert_eq!(chat.models[0].price.as_ref().unwrap().input.as_f64(), Some(1.25));
    let responses = &cfg.cloud_providers["responsebox"];
    assert_eq!(responses.api, CloudApi::OpenAiResponses);
    assert!(responses.models.is_empty());
    let messages = &cfg.cloud_providers["messagebox"];
    assert_eq!(messages.api, CloudApi::AnthropicMessages);
    assert!(messages.models[0].price.is_none());
    let debug = format!("{cfg:?}");
    assert!(!debug.contains("chat-key-ref"));
    assert!(!debug.contains("chat.example.invalid"));
}

#[test]
fn invalid_cloud_entries_are_refused_without_echoing_values() {
    use serde_json::json;

    let t = TestEnv::new();
    let dirs = Dirs::under(t.root());
    let base = json!({"api":"openai-chat", "baseUrl":"https://chat.example.invalid/v1",
        "key":"test-key-ref", "pool":"api-monthly", "models":[{"id":"known", "context":32768}]});
    for (field, value) in [
        ("apiKey", json!("inline-value-test-marker")),
        ("api", json!("unrecognized-test-protocol")),
        ("key", json!("$TEST_KEY_REFERENCE")),
        ("baseUrl", json!("http://chat.example.invalid/v1")),
        (
            "baseUrl",
            json!("https://inline-value-test-marker@chat.example.invalid/v1"),
        ),
        (
            "baseUrl",
            json!("https://chat.example.invalid/v1?key=inline-value-test-marker"),
        ),
        ("models", json!([{"id":"known", "context":0}])),
        (
            "models",
            json!([{"id":"known", "context":32768,"price":{"input":-1,"output":4}}]),
        ),
        (
            "models",
            json!([{"id":"known", "context":32768},{"id":"known", "context":32768}]),
        ),
    ] {
        let mut entry = base.clone();
        entry[field] = value;
        t.write(
            "home/.oxa/agent/models.json",
            &json!({"providers":{"chatbox":entry}}).to_string(),
        );
        let (cfg, diags) = ModelsConfig::load(&dirs, &Env::from_pairs(&[]));
        assert!(
            cfg.cloud_providers.is_empty(),
            "invalid field {field} was retained"
        );
        assert_eq!(cfg.default, "oxe");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].message,
            "Invalid HTTPS cloud provider configuration; skipped"
        );
    }
}

#[test]
fn bad_models_json_is_a_diagnostic_not_a_failure() {
    let t = TestEnv::new();
    let dirs = Dirs::under(t.root());
    t.write("home/.oxa/agent/models.json", "{ not json");
    let (cfg, diags) = ModelsConfig::load(&dirs, &Env::from_pairs(&[]));
    assert_eq!(cfg.endpoints.len(), 1);
    assert_eq!(cfg.default, "oxe");
    assert_eq!(diags.len(), 1);
}

#[test]
fn env_empty_values_count_as_unset() {
    let env = Env::from_pairs(&[("OXA_ENGINE_URL", "")]);
    assert_eq!(env.get("OXA_ENGINE_URL"), None);
    let t = TestEnv::new();
    let (cfg, _) = ModelsConfig::load(&Dirs::under(t.root()), &env);
    assert_eq!(cfg.endpoints[0].base_url, "http://127.0.0.1:8092");
}

/// issue #50: engine model ids that contain a slash are not `provider/id`.
#[test]
fn slash_model_id_beats_provider_prefix() {
    let models = vec![mr("oxe", "anthropic/claude-sonnet-5-5"), mr("oxe", "flash-20b")];
    assert_eq!(
        resolve_model("anthropic/claude-sonnet-5-5", None, &models).unwrap(),
        mr("oxe", "anthropic/claude-sonnet-5-5")
    );
    let m = resolve_model("anthropic/claude-sonnet-5-5:high", None, &models).unwrap();
    assert_eq!(m.model, "anthropic/claude-sonnet-5-5");
    assert_eq!(m.thinking, Some(ThinkingLevel::High));
    // no such model: the prefix is still treated as an unsupported provider
    let err = resolve_model("anthropic/other", None, &models).unwrap_err();
    assert_eq!(err.code, codes::UNSUPPORTED_IN_OXA);
}

/// Models with no endpoint name (the interactive `/model` list) show their ids as `/model <id>`
/// takes them, without a stray leading slash.
#[test]
fn not_found_hint_lists_ids_without_a_leading_slash_for_an_empty_endpoint() {
    let models = vec![mr("", "local-default"), mr("", "anthropic/claude-haiku-5-5")];
    let err = resolve_model("nosuch", None, &models).unwrap_err();
    let hint = err.hint.expect("a hint");
    assert_eq!(hint, "available: local-default, anthropic/claude-haiku-5-5");
}

/// #50: an oxc model id with a slash (`anthropic/...`) is a model id, not an oxa provider.
#[test]
fn slash_model_id_is_not_taken_as_a_provider() {
    let available = vec![ModelRef {
        endpoint: "oxe".to_string(),
        model: "anthropic/claude-sonnet-5-5".to_string(),
        thinking: None,
    }];
    let got = resolve_model("anthropic/claude-sonnet-5-5", None, &available).unwrap();
    assert_eq!(got.model, "anthropic/claude-sonnet-5-5");
    let got = resolve_model("anthropic/claude-sonnet-5-5:high", None, &available).unwrap();
    assert_eq!(got.model, "anthropic/claude-sonnet-5-5");
}

/// #349: with a single endpoint the hint lists the bare ids the picker and `--model` use.
#[test]
fn not_found_hint_lists_bare_ids_for_a_single_endpoint() {
    let models = vec![mr("oxe", "anthropic/claude-haiku-5-5"), mr("oxe", "flash-20b")];
    let err = resolve_model("nope/zzz", Some("oxe"), &models).unwrap_err();
    assert_eq!(
        err.hint.unwrap(),
        "available: anthropic/claude-haiku-5-5, flash-20b"
    );
}

/// #381: a near miss gets a "did you mean" with the id the user can paste back.
#[test]
fn not_found_hint_suggests_the_closest_model() {
    let models = vec![
        mr("oxe", "local-default"),
        mr("oxe", "anthropic/claude-haiku-5-5"),
    ];
    for typo in ["claude-haiku-5-6", "local-defualt"] {
        let hint = resolve_model(typo, None, &models)
            .unwrap_err()
            .hint
            .expect("a hint");
        assert!(hint.contains("did you mean"), "{typo}: {hint}");
    }
    let hint = resolve_model("local-defualt", None, &models)
        .unwrap_err()
        .hint
        .unwrap();
    assert!(hint.contains("did you mean local-default?"), "{hint}");
}
