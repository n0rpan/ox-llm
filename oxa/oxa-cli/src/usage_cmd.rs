//! `oxa usage --json`: today's cloud spend and request sums per usage pool, read from the private
//! ledger file only. The output holds numbers and configured caps. No key, salt, ledger row,
//! session id, digest or model text is read into it.
use oxa_config::{Dirs, Settings};
use oxa_proto::{codes, OxaError};
use oxc_gateway::ledger::{self, Today};
use oxc_gateway::openai::subscription_config;
use oxc_gateway::spend::{self, DaySums};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

/// The share of a configured cap from which a pool reads "low". Owned by oxa: the panel holds
/// no threshold of its own.
const LOW_SHARE: f64 = 0.8;

/// The user-only caps and the providers each pool serves, from `oxa.cloud`.
#[derive(Debug, Default, PartialEq)]
struct Caps {
    budget: Option<f64>,
    /// (daily requests, daily tokens): both or neither, as the gateway requires.
    subscription: Option<(u64, u64)>,
    metered_providers: BTreeSet<String>,
    subscription_providers: BTreeSet<String>,
}

pub(crate) fn run(words: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    if words.len() != 1 || words[0] != "--json" {
        return Err(OxaError::new(codes::USAGE, "usage: oxa usage --json"));
    }
    let settings = Settings::load(dirs, cwd, false);
    let today = ledger::today(&dirs.agent_dir.join("cloud-ledger"));
    let (caps, sums) = match caps(&settings).and_then(|caps| read_today(&today).map(|sums| (caps, sums))) {
        Ok(pair) => pair,
        Err(code) => return fail(out, code),
    };
    let value = report(sums, &caps, &today);
    serde_json::to_writer(&mut *out, &value).map_err(|e| OxaError::new(codes::IO, e.to_string()))?;
    out.write_all(b"\n")
        .map_err(|e| OxaError::io("cannot write usage", &e))?;
    Ok(0)
}

/// The panel's fault object: one JSON line on stdout, exit 1.
fn fail(out: &mut dyn Write, code: &'static str) -> Result<i32, OxaError> {
    let value = json!({"type": "oxa_error", "version": 1, "command": "usage", "code": code});
    serde_json::to_writer(&mut *out, &value).map_err(|e| OxaError::new(codes::IO, e.to_string()))?;
    out.write_all(b"\n")
        .map_err(|e| OxaError::io("cannot write usage error", &e))?;
    Ok(1)
}

fn read_today(today: &Today) -> Result<Option<DaySums>, &'static str> {
    spend::day_sums(&today.path).map_err(|_| "ledger_unreadable")
}

fn caps(settings: &Settings) -> Result<Caps, &'static str> {
    let cloud = settings.get("oxa.cloud").cloned().unwrap_or(Value::Null);
    let budget = match cloud.get("daily_budget_usd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
                .ok_or("config_invalid")?,
        ),
    };
    let subscription = subscription_config::parse(&cloud, false)
        .map_err(|_| "config_invalid")?
        .limits;
    let mut caps = Caps {
        budget,
        subscription,
        ..Caps::default()
    };
    if let Some(models) = cloud.get("models").and_then(Value::as_object) {
        for model in models.values() {
            let Some(provider) = model.get("provider").and_then(Value::as_str) else {
                continue;
            };
            if matches!(provider, "chatgpt" | "opencode") {
                caps.subscription_providers.insert(provider.to_owned());
            } else {
                caps.metered_providers.insert(provider.to_owned());
            }
        }
    }
    if cloud.pointer("/opencode/enabled") == Some(&Value::Bool(true)) {
        caps.subscription_providers.insert("opencode".to_owned());
    }
    Ok(caps)
}

/// `ok`, `low` or `over` for `used` against a cap of `cap` (both in the same unit).
fn share_state(used: f64, cap: f64) -> &'static str {
    if used >= cap {
        "over"
    } else if used >= LOW_SHARE * cap {
        "low"
    } else {
        "ok"
    }
}

fn providers(set: &BTreeSet<String>) -> Value {
    json!(set.iter().collect::<Vec<_>>())
}

fn report(sums: Option<DaySums>, caps: &Caps, today: &Today) -> Value {
    let s = sums.unwrap_or_default();
    let (metered_state, metered_enforcement) = match caps.budget {
        None => ("none", "none"),
        Some(_) if s.metered_cost_unknown => ("unknown", "refuse"),
        Some(cap) => (share_state(s.metered_usd, cap), "refuse"),
    };
    let (subscription_state, subscription_enforcement) = match caps.subscription {
        None => ("none", "none"),
        Some((request_cap, token_cap)) => {
            let share = (s.subscription_requests as f64 / request_cap as f64)
                .max(s.subscription_tokens as f64 / token_cap as f64);
            (share_state(share, 1.0), "refuse")
        }
    };
    let ledger = if sums.is_some() { "ok" } else { "absent" };
    json!({
        "type": "oxa_usage", "version": 1,
        "day": today.day, "resets_ms": today.resets_ms,
        "ledger": ledger,
        "pools": [
            {
                "id": "metered", "billing": "metered",
                "providers": providers(&caps.metered_providers),
                "state": metered_state, "enforcement": metered_enforcement,
                "spent_usd": s.metered_usd, "cost_unknown": s.metered_cost_unknown,
                "budget_usd": caps.budget,
                "requests": s.metered_requests,
                "tokens": {"prompt": s.metered_prompt, "completion": s.metered_completion, "cached": s.metered_cached},
                "credit": null, "windows": []
            },
            {
                "id": "subscription", "billing": "subscription",
                "providers": providers(&caps.subscription_providers),
                "state": subscription_state, "enforcement": subscription_enforcement,
                "requests": s.subscription_requests,
                "request_cap": caps.subscription.map(|(requests, _)| requests),
                "tokens_used": s.subscription_tokens,
                "token_cap": caps.subscription.map(|(_, tokens)| tokens),
                "windows": []
            },
            {
                "id": "local", "billing": "local", "providers": ["local"],
                "state": "none", "enforcement": "none",
                "requests": s.local_requests,
                "tokens": {"prompt": s.local_prompt, "completion": s.local_completion, "cached": 0}
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_discoverable_as_a_json_only_cli_command() {
        use crate::args::{parse_args, usage_text, Subcommand};
        use oxa_interactive::command_registry::{self, Argument, ArgumentKind, Availability, Surface};

        let argv = ["usage".to_owned(), "--json".to_owned()];
        let command = command_registry::parse_cli(&argv).expect("usage is registered");
        assert_eq!(command.command.path, ["usage"]);
        assert_eq!(
            command.command.arguments,
            &[Argument {
                name: "json",
                kind: ArgumentKind::Flag,
                required: true
            }]
        );
        for surface in [Surface::Slash, Surface::Menu, Surface::Web] {
            assert!(matches!(
                command.command.surfaces.at(surface),
                Availability::Exception(_)
            ));
        }
        assert!(usage_text(&[])
            .lines()
            .any(|line| line.starts_with("  oxa usage ")));
        assert!(crate::help::text(&["usage"])
            .unwrap()
            .contains("oxa usage --json"));
        let parsed = parse_args(&argv, &[]).unwrap();
        assert!(matches!(
            parsed.subcommand,
            Some(Subcommand::Registered(id, words)) if id == command.command.id && words.0 == argv[1..]
        ));
    }
    use std::fs;

    const FIXTURE: &str = include_str!("../../omarchy-panel/tests/fixtures/usage.json");

    fn today() -> Today {
        Today {
            day: "2026-11-19".into(),
            path: "/unused/requests-20261119.jsonl".into(),
            resets_ms: Some(1_795_132_800_000),
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oxa-usage-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn keys(value: &Value) -> BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn usage_json_matches_fixture_shape() {
        let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
        let sums = DaySums {
            metered_requests: 2,
            metered_usd: 11.2,
            subscription_requests: 1,
            subscription_tokens: 10,
            ..DaySums::default()
        };
        let caps = Caps {
            budget: Some(16.0),
            subscription: Some((300, 5_000_000)),
            ..Caps::default()
        };
        let value = report(Some(sums), &caps, &today());
        assert!(
            keys(&value).is_subset(&keys(&fixture)),
            "top-level keys match the fixture"
        );
        assert_eq!(value["type"], fixture["type"]);
        assert_eq!(value["version"], fixture["version"]);
        let ours = value["pools"].as_array().unwrap();
        let theirs = fixture["pools"].as_array().unwrap();
        assert_eq!(ours.len(), theirs.len());
        for (mine, theirs) in ours.iter().zip(theirs) {
            assert_eq!(mine["id"], theirs["id"], "pool order follows the fixture");
            assert!(keys(mine).is_subset(&keys(theirs)), "pool keys match the fixture");
        }
    }

    #[test]
    fn usage_without_budget_is_state_none() {
        let sums = DaySums {
            metered_usd: 3.0,
            ..DaySums::default()
        };
        let none = report(Some(sums), &Caps::default(), &today());
        assert_eq!(none["pools"][0]["state"], "none");
        assert_eq!(none["pools"][0]["enforcement"], "none");
        assert_eq!(none["pools"][0]["budget_usd"], Value::Null);
        assert_eq!(none["pools"][1]["state"], "none");
        let capped = Caps {
            budget: Some(16.0),
            ..Caps::default()
        };
        let low = report(
            Some(DaySums {
                metered_usd: 13.0,
                ..DaySums::default()
            }),
            &capped,
            &today(),
        );
        assert_eq!(low["pools"][0]["state"], "low");
        let over = report(
            Some(DaySums {
                metered_usd: 16.0,
                ..DaySums::default()
            }),
            &capped,
            &today(),
        );
        assert_eq!(over["pools"][0]["state"], "over");
        let unknown = report(
            Some(DaySums {
                metered_cost_unknown: true,
                ..DaySums::default()
            }),
            &capped,
            &today(),
        );
        assert_eq!(unknown["pools"][0]["state"], "unknown");
    }

    #[test]
    fn usage_has_only_sums() {
        let dir = scratch("sums");
        let path = dir.join("requests-20261119.jsonl");
        let rows = [
            json!({"remote": true, "remote_provider": "openai", "billing": "metered", "cost_usd": 1.25,
                "settled": "usage", "session": "secret-session-id", "model": "model-with-text",
                "remote_model": "gpt-x", "prompt_tokens": 10, "completion_tokens": 4, "cached_tokens": 2}),
            json!({"remote": true, "remote_provider": "chatgpt", "billing": "subscription", "cost_usd": 0,
                "settled": "usage", "subscription_requests": 1, "subscription_tokens": 100,
                "session": "secret-session-id"}),
            json!({"remote": false, "origin": "api", "session": "secret-session-id",
                "prompt_tokens": 3, "completion_tokens": 1}),
        ];
        let text: String = rows.iter().map(|row| format!("{row}\n")).collect();
        fs::write(&path, text).unwrap();
        let sums = spend::day_sums(&path).unwrap().unwrap();
        let output = serde_json::to_string(&report(Some(sums), &Caps::default(), &today())).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(!output.contains("secret-session-id"), "no session id: {output}");
        assert!(!output.contains("model-with-text"), "no model text: {output}");
        assert!(!output.contains("gpt-x"), "no remote model name: {output}");
        assert_eq!(sums.metered_requests, 1);
        assert_eq!(sums.metered_prompt, 10);
        assert_eq!(sums.subscription_tokens, 100);
        assert_eq!(sums.local_requests, 1);
    }

    #[test]
    fn absent_ledger_is_zero_not_error() {
        let dir = scratch("absent");
        let path = dir.join("requests-20261119.jsonl");
        assert_eq!(spend::day_sums(&path).unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
        let capped = Caps {
            budget: Some(16.0),
            ..Caps::default()
        };
        let value = report(None, &capped, &today());
        assert_eq!(value["ledger"], "absent");
        assert_eq!(value["pools"][0]["spent_usd"], 0.0);
        assert_eq!(value["pools"][0]["state"], "ok");
        assert_eq!(value["pools"][1]["requests"], 0);
    }

    #[test]
    fn caps_come_from_user_settings_only_and_bad_budget_is_invalid() {
        let settings = Settings::from_values(
            json!({"oxa": {"cloud": {
                "daily_budget_usd": 16,
                "subscription_limits": {"daily_requests": 300, "daily_tokens": 5000000},
                "models": {"a": {"provider": "openai"}, "b": {"provider": "chatgpt"}}
            }}}),
            json!({}),
        );
        let parsed = caps(&settings).unwrap();
        assert_eq!(parsed.budget, Some(16.0));
        assert_eq!(parsed.subscription, Some((300, 5_000_000)));
        assert!(parsed.metered_providers.contains("openai"));
        assert!(parsed.subscription_providers.contains("chatgpt"));
        let bad = Settings::from_values(json!({"oxa": {"cloud": {"daily_budget_usd": "lots"}}}), json!({}));
        assert_eq!(caps(&bad), Err("config_invalid"));
    }
}
