//! Keyed defaults of the native cloud gateway.
//!
//! A provider key (or the ChatGPT sign-in) in the owner's protected store opens the gateway and
//! its model discovery without `oxa.engine.inProcessCloud` / `oxa.cloud.discover_models`. There is
//! no built-in model table: the model list comes live from each provider's models endpoint with
//! the owner's key, limits and prices the provider does not give come from the public OpenRouter
//! catalog (no key sent), and the last successful fetch is kept in the on-disk catalog cache. A
//! model nobody can price is usable with unknown cost, recorded as uncertain spend.
use oxa_config::{Dirs, Settings};
use oxc_gateway::{catalog::Provider, secrets};
use serde_json::Value;
use std::borrow::Cow;

fn keyed(dirs: &Dirs, provider: Provider) -> bool {
    secrets::read(&super::credential_path(dirs), provider.as_str()).is_ok()
}

fn signed_in_chatgpt(dirs: &Dirs) -> bool {
    super::credential_path(dirs)
        .parent()
        .is_some_and(|dir| dir.join("chatgpt-auth.json").is_file())
}

/// The owner's user settings with the keyed defaults applied, in memory only.
///
/// * `oxa.engine.inProcessCloud` absent and a key or sign-in present: the gateway opens and
///   `oxa.cloud.discover_models` defaults to true. Any explicit value (false opts out) wins.
/// * Offline mode, a mistyped setting and a non-object `oxa.cloud` leave the settings alone, so
///   their usual refusals stay.
pub(crate) fn with_keyed_defaults<'a>(user: &'a Settings, dirs: &Dirs) -> Cow<'a, Settings> {
    let explicit = user.get(super::SETTING);
    if oxa_proto::offline() || explicit.is_some_and(|value| value != &Value::Bool(true)) {
        return Cow::Borrowed(user);
    }
    // An owner who switched Go on explicitly chose a Go-only gateway; keys do not widen it.
    if explicit.is_none() && user.get(super::GO_SETTING) == Some(&Value::Bool(true)) {
        return Cow::Borrowed(user);
    }
    let cloud = user.get("oxa.cloud");
    if cloud.is_some_and(|value| !value.is_object() && !value.is_null()) {
        return Cow::Borrowed(user);
    }
    // A mistyped catalog keeps its usual refusal.
    if user
        .get("oxa.cloud.models")
        .is_some_and(|value| !value.is_object() && !value.is_null())
    {
        return Cow::Borrowed(user);
    }
    let api_keyed = [Provider::OpenRouter, Provider::OpenAi, Provider::Anthropic]
        .into_iter()
        .any(|provider| keyed(dirs, provider));
    if explicit.is_some() || !(api_keyed || signed_in_chatgpt(dirs)) {
        return Cow::Borrowed(user);
    }
    let mut out = user.clone().with_override(super::SETTING, Value::Bool(true));
    if user.get("oxa.cloud.discover_models").is_none() {
        out = out.with_override("oxa.cloud.discover_models", Value::Bool(true));
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
        Settings::from_values(value, serde_json::json!({}))
    }

    #[test]
    fn a_key_opens_the_gateway_with_discovery_and_no_built_in_models() {
        let (dirs, root) = home("anthropic");
        write_keys(&root, r#"{"anthropic":"synthetic-key"}"#);
        let settings = user(serde_json::json!({}));
        let applied = with_keyed_defaults(&settings, &dirs);
        assert!(applied.bool("oxa.engine.inProcessCloud", false));
        assert!(applied.bool("oxa.cloud.discover_models", false));
        assert!(applied.get("oxa.cloud.models").is_none());
    }

    #[test]
    fn a_chatgpt_sign_in_opens_the_gateway() {
        let (dirs, root) = home("chatgpt");
        std::fs::write(root.join(".config/oxc/llm/chatgpt-auth.json"), "{}").unwrap();
        let settings = user(serde_json::json!({}));
        assert!(cloud_enabled(&settings, &dirs));
    }

    #[test]
    fn explicit_owner_settings_win_and_owner_models_are_kept() {
        let (dirs, root) = home("explicit");
        write_keys(&root, r#"{"anthropic":"synthetic-key","openai":"synthetic-key"}"#);
        let off = user(serde_json::json!({"oxa":{"engine":{"inProcessCloud":false}}}));
        let applied = with_keyed_defaults(&off, &dirs);
        assert!(!applied.bool("oxa.engine.inProcessCloud", true));
        let own = serde_json::json!({"provider":"openai","upstream_model":"m","context_window":1000,"max_output":10,
            "pricing":{"input_per_million":1,"output_per_million":2}});
        let settings = user(
            serde_json::json!({"oxa":{"cloud":{"discover_models":false,"models":{"openai/m":own.clone()}}}}),
        );
        let applied = with_keyed_defaults(&settings, &dirs);
        assert!(!applied.bool("oxa.cloud.discover_models", true));
        assert_eq!(applied.get("oxa.cloud.models").unwrap()["openai/m"], own);
    }

    #[test]
    fn no_key_and_no_sign_in_change_nothing() {
        let (dirs, _root) = home("none");
        let settings = user(serde_json::json!({}));
        assert!(matches!(with_keyed_defaults(&settings, &dirs), Cow::Borrowed(_)));
        assert!(!cloud_enabled(&settings, &dirs));
    }
}
