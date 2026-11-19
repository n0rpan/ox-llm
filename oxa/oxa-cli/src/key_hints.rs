//! First-run key recommendations: which optional capabilities have no key yet, and the exact
//! way to add each one. Shown once in the interactive terminal and never blocking. Only the
//! presence of a key is checked; no key value reaches a message, a log or the environment.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, Level, OxaError};
use oxc_gateway::catalog::Provider;
use std::path::Path;

/// Written in the agent directory once the recommendations were shown.
const MARKER: &str = "key-hints-shown";
/// The owner's ChatGPT sign-in record, beside the cloud key file.
const CHATGPT_RECORD: &str = ".config/oxc/llm/chatgpt-auth.json";

/// What has no key yet.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Missing {
    web_search: bool,
    /// Names of the cloud providers without an entry in the cloud key file.
    cloud: Vec<&'static str>,
    chatgpt: bool,
}

/// The recommendations for this start, or `None` when they were shown before, nothing is
/// missing, the run is offline or the owner asked for a quiet start.
pub(crate) fn first_run_notice(dirs: &Dirs, env: &Env, settings: &Settings) -> Option<(Level, String)> {
    let quiet = settings
        .get("quietStartup")
        .is_some_and(|value| value.as_bool() != Some(false));
    if quiet || oxa_proto::offline() {
        return None;
    }
    let marker = dirs.agent_dir.join(MARKER);
    if marker.exists() {
        return None;
    }
    let text = text(&detect(dirs, env, settings), &shown(&dirs.agent_dir, &dirs.home))?;
    // Best effort: when the marker cannot be written the note is shown again at the next start.
    let _ = std::fs::create_dir_all(&dirs.agent_dir).and_then(|()| std::fs::write(&marker, b""));
    Some((Level::Info, text))
}

/// Shown under the "cannot reach the engine" error of an install that names no engine.
const NO_MODEL_HELP: &str = "No model is set up yet. Any one of these gives oxa a model:
- OpenCode Go: an `opencode` entry in ~/.config/oxc/llm/cloud-secrets.json (mode 0600) is enough
- OpenRouter, OpenAI, Anthropic: a key in the same file, plus the gateway settings in the quickstart
- ChatGPT plan: oxa login chatgpt --experimental, then a model route (quickstart)
- a local or other engine: set OXA_ENGINE_URL to its address
The quickstart (\"Cloud models\", \"Local models\") has each step.";

/// The ways to get a model, for an interactive start that reached no engine while nothing names
/// one. `None` for any other failure and when the owner named an engine (it is just down).
pub(crate) fn no_model_help(error: &OxaError, dirs: &Dirs, env: &Env) -> Option<&'static str> {
    let named = env.get("OXA_ENGINE_URL").is_some() || dirs.models_file().exists();
    (error.code == codes::ENGINE_DOWN && !named).then_some(NO_MODEL_HELP)
}

fn detect(dirs: &Dirs, env: &Env, settings: &Settings) -> Missing {
    let web_wanted = settings.bool(oxa_web::SETTING_ENABLED, true)
        && settings.string(oxa_web::SETTING_PROVIDER).as_deref() != Some("none");
    let web_key = env.get(oxa_web::ENV_KEY).is_some_and(|key| !key.is_empty())
        || dirs.agent_dir.join(oxa_web::KEY_FILE).exists();
    let secrets = crate::in_process::credential_path(dirs);
    let cloud = [
        (Provider::OpenCode, "OpenCode Go"),
        (Provider::OpenRouter, "OpenRouter"),
        (Provider::OpenAi, "OpenAI"),
        (Provider::Anthropic, "Anthropic"),
    ]
    .into_iter()
    .filter(|(provider, _)| oxc_gateway::secrets::read(&secrets, provider.as_str()).is_err())
    .map(|(_, name)| name)
    .collect();
    Missing {
        web_search: web_wanted && !web_key,
        cloud,
        chatgpt: !dirs.home.join(CHATGPT_RECORD).exists(),
    }
}

/// `path` with the home directory written as `~`.
fn shown(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn text(missing: &Missing, agent_dir: &str) -> Option<String> {
    if !missing.web_search && missing.cloud.is_empty() && !missing.chatgpt {
        return None;
    }
    let mut lines = vec!["Optional keys not set up yet (shown once; nothing is blocked):".to_string()];
    if missing.web_search {
        lines.push(format!(
            "- web_search is not offered without a Brave Search key: {agent_dir}/{} (mode 0600)",
            oxa_web::KEY_FILE
        ));
    }
    if !missing.cloud.is_empty() {
        lines.push(format!(
            "- cloud models ({}): ~/.config/oxc/llm/cloud-secrets.json (mode 0600)",
            missing.cloud.join(", ")
        ));
    }
    if missing.chatgpt {
        lines.push("- ChatGPT plan: oxa login chatgpt --experimental".to_string());
    }
    lines.push(
        "How to add a key without showing it: quickstart, \"Keys\". Never in settings.json.".to_string(),
    );
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use serde_json::json;

    fn none() -> Settings {
        Settings::from_values(json!({}), json!({}))
    }

    #[test]
    fn a_fresh_install_is_told_once_and_a_present_key_is_not_named() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let vars = Env::from_pairs(&[]);
        let (level, first) = first_run_notice(&dirs, &vars, &none()).expect("a fresh install gets the note");
        assert_eq!(level, Level::Info);
        for name in [
            "web_search",
            "OpenCode Go",
            "OpenRouter",
            "OpenAI",
            "Anthropic",
            "ChatGPT",
        ] {
            assert!(first.contains(name), "{name} is missing from: {first}");
        }
        assert_eq!(first_run_notice(&dirs, &vars, &none()), None, "shown once");

        let other = TestEnv::new();
        let dirs = Dirs::under(other.root());
        let keyed = Env::from_pairs(&[(oxa_web::ENV_KEY, "synthetic-not-a-key")]);
        let (_, text) = first_run_notice(&dirs, &keyed, &none()).expect("cloud keys are still missing");
        assert!(!text.contains("web_search"), "{text}");
        assert!(
            !text.contains("synthetic-not-a-key"),
            "a key value is never shown"
        );
    }

    #[test]
    fn an_unreachable_default_engine_gets_the_setup_steps_and_a_named_one_does_not() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let down = OxaError::new(codes::ENGINE_DOWN, "cannot reach the engine");
        let help = no_model_help(&down, &dirs, &Env::from_pairs(&[])).expect("setup steps");
        assert!(
            help.contains("cloud-secrets.json") && help.contains("OXA_ENGINE_URL"),
            "{help}"
        );
        let named = Env::from_pairs(&[("OXA_ENGINE_URL", "http://127.0.0.1:8080")]);
        assert_eq!(no_model_help(&down, &dirs, &named), None);
        let other = OxaError::new(codes::USAGE, "usage");
        assert_eq!(no_model_help(&other, &dirs, &Env::from_pairs(&[])), None);
    }

    #[test]
    fn a_quiet_start_and_a_complete_setup_show_nothing() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let quiet = Settings::from_values(json!({"quietStartup": true}), json!({}));
        assert_eq!(first_run_notice(&dirs, &Env::from_pairs(&[]), &quiet), None);
        assert_eq!(text(&Missing::default(), "~/.oxa/agent"), None);
    }
}
