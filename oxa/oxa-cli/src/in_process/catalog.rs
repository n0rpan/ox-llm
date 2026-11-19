//! Built-in recommended models and the keyed defaults of the native cloud gateway.
//!
//! A provider key (or the ChatGPT sign-in) in the owner's protected store opens the gateway and
//! its model discovery without `oxa.engine.inProcessCloud` / `oxa.cloud.discover_models`, and the
//! recommended models below sit under the owner's own `oxa.cloud.models`. Aliases are
//! `provider/upstream`, the same names discovery generates, so a discovered offer refines the
//! built-in limits and price instead of duplicating it. Prices and limits go stale: discovery
//! (OpenRouter, OpenAI, Anthropic) and the owner's own entry override them, and an unpriced model
//! is admitted and recorded with unknown cost rather than dropped.
use oxa_config::{Dirs, Settings};
use oxc_gateway::{catalog::Provider, secrets};
use serde_json::{json, Map, Value};
use std::borrow::Cow;

fn keyed(dirs: &Dirs, provider: Provider) -> bool {
    secrets::read(&super::credential_path(dirs), provider.as_str()).is_ok()
}

fn signed_in_chatgpt(dirs: &Dirs) -> bool {
    super::credential_path(dirs)
        .parent()
        .is_some_and(|dir| dir.join("chatgpt-auth.json").is_file())
}

fn route(
    provider: &str,
    upstream: &str,
    context: u32,
    output: u32,
    pricing: Option<Value>,
) -> (String, Value) {
    let mut value = json!({
        "provider": provider,
        "upstream_model": upstream,
        "context_window": context,
        "max_output": output,
    });
    if let Some(pricing) = pricing {
        value["pricing"] = pricing;
    }
    (format!("{provider}/{upstream}"), value)
}

fn rates(input: f64, output: f64, read: f64, write: f64) -> Value {
    json!({
        "input_per_million": input,
        "output_per_million": output,
        "cache_read_per_million": read,
        "cache_write_per_million": write,
    })
}

fn anthropic() -> Vec<(String, Value)> {
    let flat = |input: f64, output: f64| Some(rates(input, output, input * 0.1, input * 1.25));
    let haiku = |input: f64, output: f64, max: Value| {
        let mut tier = rates(input, output, input * 0.1, input * 1.25);
        tier["max_prompt_tokens"] = max;
        tier
    };
    vec![
        route(
            "anthropic",
            "claude-opus-5-5",
            1_000_000,
            128_000,
            flat(4.0, 20.0),
        ),
        route(
            "anthropic",
            "claude-sonnet-5-5",
            1_000_000,
            128_000,
            flat(2.0, 10.0),
        ),
        route(
            "anthropic",
            "claude-haiku-5-5",
            1_000_000,
            128_000,
            Some(json!({"tiers": [haiku(0.1, 0.5, json!(100_000)), haiku(0.5, 2.5, Value::Null)]})),
        ),
        route(
            "anthropic",
            "claude-opus-4-8",
            1_000_000,
            128_000,
            flat(5.0, 25.0),
        ),
        route(
            "anthropic",
            "claude-sonnet-4-6",
            1_000_000,
            128_000,
            flat(3.0, 15.0),
        ),
        route("anthropic", "claude-haiku-4-5", 200_000, 64_000, flat(1.0, 5.0)),
    ]
}

fn openai() -> Vec<(String, Value)> {
    let priced = |input: f64, output: f64| Some(rates(input, output, input * 0.1, input));
    vec![
        route("openai", "gpt-5", 400_000, 128_000, priced(1.25, 10.0)),
        route("openai", "gpt-5-mini", 400_000, 128_000, priced(0.25, 2.0)),
        route("openai", "gpt-5-nano", 400_000, 128_000, priced(0.05, 0.4)),
    ]
}

/// Subscription routes carry no price; the plan's limits are the owner's advisory setting.
fn chatgpt() -> Vec<(String, Value)> {
    vec![
        route("chatgpt", "gpt-5", 400_000, 128_000, None),
        route("chatgpt", "gpt-5-codex", 400_000, 128_000, None),
    ]
}

/// The owner's user settings with the keyed defaults applied, in memory only.
///
/// * `oxa.engine.inProcessCloud` absent and a key or sign-in present: the gateway opens and
///   `oxa.cloud.discover_models` defaults to true. Any explicit value (false opts out) wins.
/// * The recommended models of each keyed provider fill in under `oxa.cloud.models`; an owner
///   entry of the same alias is never replaced.
/// * Offline mode, a mistyped setting and a non-object `oxa.cloud` leave the settings alone, so
///   their usual refusals stay.
pub(crate) fn with_keyed_defaults<'a>(user: &'a Settings, dirs: &Dirs) -> Cow<'a, Settings> {
    let explicit = user.get(super::SETTING);
    if oxa_proto::offline() || explicit.is_some_and(|value| value != &Value::Bool(true)) {
        return Cow::Borrowed(user);
    }
    let cloud = user.get("oxa.cloud");
    if cloud.is_some_and(|value| !value.is_object() && !value.is_null()) {
        return Cow::Borrowed(user);
    }
    let api_keyed = [Provider::OpenRouter, Provider::OpenAi, Provider::Anthropic]
        .into_iter()
        .any(|provider| keyed(dirs, provider));
    let signed_in = signed_in_chatgpt(dirs);
    if explicit.is_none() && !api_keyed && !signed_in {
        return Cow::Borrowed(user);
    }
    let mut routes = Vec::new();
    if keyed(dirs, Provider::Anthropic) {
        routes.extend(anthropic());
    }
    if keyed(dirs, Provider::OpenAi) {
        routes.extend(openai());
    }
    if signed_in {
        routes.extend(chatgpt());
    }
    let mut out = user.clone();
    if !routes.is_empty() {
        let mut models: Map<String, Value> = match user.get("oxa.cloud.models") {
            Some(Value::Object(models)) => models.clone(),
            Some(Value::Null) | None => Map::new(),
            // A mistyped catalog keeps its usual refusal.
            Some(_) => return Cow::Borrowed(user),
        };
        for (alias, route) in routes {
            models.entry(alias).or_insert(route);
        }
        out = out.with_override("oxa.cloud.models", Value::Object(models));
    }
    if explicit.is_none() {
        out = out.with_override(super::SETTING, Value::Bool(true));
        if user.get("oxa.cloud.discover_models").is_none() {
            out = out.with_override("oxa.cloud.discover_models", Value::Bool(true));
        }
    }
    Cow::Owned(out)
}

/// Whether the native gateway is the owner's transport, explicitly or through a key.
pub(crate) fn cloud_enabled(user: &Settings, dirs: &Dirs) -> bool {
    with_keyed_defaults(user, dirs).bool(super::SETTING, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_gateway::{catalog::Model, pricing::Price};
    use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

    fn home(name: &str) -> (Dirs, PathBuf) {
        let root = PathBuf::from("/tmp").join(format!("oxa-keyed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".config/oxc/llm")).unwrap();
        let dirs = Dirs {
            home: root.clone(),
            agent_dir: root.join(".oxa/agent"),
            sessions_dir: root.join(".oxa/agent/sessions"),
            tmp_dir: root.join(".oxa/agent/tmp"),
            system_skills_dir: None,
        };
        (dirs, root)
    }

    fn write_keys(root: &std::path::Path, body: &str) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(root.join(".config/oxc/llm/cloud-secrets.json"))
            .unwrap();
        file.write_all(body.as_bytes()).unwrap();
    }

    fn user(value: Value) -> Settings {
        Settings::from_values(value, json!({}))
    }

    #[test]
    fn a_key_opens_the_gateway_with_discovery_and_the_recommended_models() {
        let (dirs, root) = home("anthropic");
        write_keys(&root, r#"{"anthropic":"synthetic-key"}"#);
        let settings = user(json!({}));
        let applied = with_keyed_defaults(&settings, &dirs);
        assert!(applied.bool("oxa.engine.inProcessCloud", false));
        assert!(applied.bool("oxa.cloud.discover_models", false));
        let models = applied
            .get("oxa.cloud.models")
            .and_then(Value::as_object)
            .unwrap();
        assert!(models.contains_key("anthropic/claude-opus-5-5"));
        // Only the keyed provider's models appear.
        assert!(models.keys().all(|alias| alias.starts_with("anthropic/")));
    }

    #[test]
    fn explicit_owner_settings_win_and_owner_models_are_not_replaced() {
        let (dirs, root) = home("explicit");
        write_keys(&root, r#"{"anthropic":"synthetic-key","openai":"synthetic-key"}"#);
        let off = user(json!({"oxa":{"engine":{"inProcessCloud":false}}}));
        let applied = with_keyed_defaults(&off, &dirs);
        assert!(!applied.bool("oxa.engine.inProcessCloud", true));
        assert!(applied.get("oxa.cloud.models").is_none());
        let own = json!({"provider":"openai","upstream_model":"gpt-5","context_window":1000,"max_output":10,
            "pricing":{"input_per_million":1,"output_per_million":2}});
        let settings =
            user(json!({"oxa":{"cloud":{"discover_models":false,"models":{"openai/gpt-5":own.clone()}}}}));
        let applied = with_keyed_defaults(&settings, &dirs);
        assert!(!applied.bool("oxa.cloud.discover_models", true));
        let models = applied
            .get("oxa.cloud.models")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(models["openai/gpt-5"], own);
        assert!(models.contains_key("anthropic/claude-sonnet-5-5"));
    }

    #[test]
    fn no_key_and_no_sign_in_change_nothing() {
        let (dirs, _root) = home("none");
        let settings = user(json!({}));
        assert!(matches!(with_keyed_defaults(&settings, &dirs), Cow::Borrowed(_)));
        assert!(!cloud_enabled(&settings, &dirs));
    }

    #[test]
    fn a_chatgpt_sign_in_adds_an_unpriced_subscription_route() {
        let (dirs, root) = home("chatgpt");
        std::fs::write(root.join(".config/oxc/llm/chatgpt-auth.json"), "{}").unwrap();
        let settings = user(json!({}));
        let applied = with_keyed_defaults(&settings, &dirs);
        assert!(cloud_enabled(&settings, &dirs));
        let models = applied
            .get("oxa.cloud.models")
            .and_then(Value::as_object)
            .unwrap();
        assert!(models.values().all(|route| route["provider"] == "chatgpt"));
        assert!(!models.is_empty());
    }

    #[test]
    fn every_recommended_model_is_a_valid_priced_route() {
        for (alias, route) in anthropic().into_iter().chain(openai()) {
            let provider = if route["provider"] == "anthropic" {
                Provider::Anthropic
            } else {
                Provider::OpenAi
            };
            Model::new(
                &alias,
                provider,
                route["upstream_model"].as_str().unwrap(),
                "https://example.com",
                route["context_window"].as_u64().unwrap() as u32,
                route["max_output"].as_u64().unwrap() as u32,
            )
            .unwrap();
            assert!(Price::from_model(&route).is_some(), "{alias} has no usable price");
        }
    }
}
