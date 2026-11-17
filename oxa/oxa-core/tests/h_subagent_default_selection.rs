//! Wave H, lane subdef (O-146): the `subagent` tool is a member of oxa's DEFAULT tool selection,
//! like the seven built-ins, where the O-146 rule offers it (non-Flash and a context window of
//! 32K or more or unknown, or `oxa.subagent.enabled: true`). It is not an always-on extra: an
//! allowlist must name it, `+name` / `-name` edits keep or drop it, `--exclude-tools`,
//! `--no-builtin-tools` and `--no-tools` remove it. The tool list stays sorted.
//! Isolation: `oxa_fake::TestEnv`; the host is a mock answering `EngineLimits` only.

use oxa_config::Settings;
use oxa_core::{resolve_toolset, AgentDef, SubagentHooks, ToolSelection};
use oxa_proto::{
    codes, Access, Exposure, Hooks, Host, HostAction, OxaError, Source, Tool, ToolClass, ToolCtx, ToolOutput,
    ToolSpec, Value,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

struct Builtin(ToolSpec);

impl Tool for Builtin {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        ToolOutput::ok("")
    }
}

fn plain(name: &str, source: Source) -> Arc<dyn Tool> {
    Arc::new(Builtin(ToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        schema_json: "{}".to_string(),
        class: ToolClass::Read,
        exposure: Exposure::Direct,
        source,
    }))
}

const SEVEN: [&str; 7] = ["bash", "edit", "find", "grep", "ls", "read", "write"];

/// The engine of the session as `HostAction::EngineLimits` reports it.
struct Limits(Value);

impl Host for Limits {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        match action {
            HostAction::EngineLimits => Ok(self.0.clone()),
            _ => Err(OxaError::new(codes::INTERNAL, "not in the mock")),
        }
    }
}

#[derive(Clone, Copy)]
enum Model {
    /// The fake's default: a large non-Flash model (Rust MoE, 1M window).
    Large,
    /// A model row below 32K (`model_limits` cap 16384), not Flash.
    Small,
    /// The Rust Flash engine (`parkDiskOnly`), 65536 window.
    Flash,
    /// No limits known.
    Unknown,
}

fn limits(m: Model) -> Value {
    match m {
        Model::Large => json!({"contextWindow": 1 << 20, "parkDiskOnly": false}),
        Model::Small => json!({"contextWindow": 16384, "parkDiskOnly": false}),
        Model::Flash => json!({"contextWindow": 65536, "parkDiskOnly": true}),
        Model::Unknown => json!({}),
    }
}

/// The settings of a test: `user` layer only.
fn settings(user: Value) -> Settings {
    Settings::from_values(user, json!({}))
}

/// The built-ins, one ordinary extension tool and the subagent hook set's tools for `model`.
fn all_tools(model: Model, s: &Settings) -> Vec<Arc<dyn Tool>> {
    let mut all: Vec<Arc<dyn Tool>> = SEVEN.iter().map(|n| plain(n, Source::Builtin)).collect();
    all.push(plain("ext_tool", Source::Extension("/x/ext.js".into())));
    // `hooks.rs` does not build the hook set when the setting is false (nor in a child).
    if s.get(SubagentHooks::SETTING_ENABLED).and_then(Value::as_bool) != Some(false) {
        let agent = AgentDef {
            name: "worker".into(),
            description: "A test worker".into(),
            tools: None,
            model: None,
            system_prompt: String::new(),
            path: PathBuf::from("/agents/worker.md"),
            project: false,
        };
        let h = SubagentHooks::new(PathBuf::from("/nonexistent/oxa"), vec![agent]).with_settings(s);
        h.attach(Arc::new(Limits(limits(model))));
        all.extend(h.tools());
    }
    all
}

/// The declared names for `model`, settings `user` and the CLI part of the selection.
fn declared(model: Model, user: Value, cli: ToolSelection) -> Vec<String> {
    let s = settings(user);
    let sel = ToolSelection {
        default_tools: s.default_tools(),
        default_edits: s.default_tools_edits(),
        ..cli
    };
    let (set, _) = resolve_toolset(&all_tools(model, &s), &sel);
    set.direct.iter().map(|t| t.spec().name.clone()).collect()
}

fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

fn with(extra: &[&str]) -> Vec<String> {
    let mut out = v(&SEVEN);
    out.push("ext_tool".to_string());
    out.extend(v(extra));
    out.sort();
    out
}

fn tools(list: &[&str]) -> ToolSelection {
    ToolSelection {
        tools: Some(v(list)),
        ..Default::default()
    }
}

#[test]
fn no_selection_default_set_includes_subagent_where_o146_allows() {
    let none = || ToolSelection::default();
    assert_eq!(declared(Model::Large, json!({}), none()), with(&["subagent"]));
    assert_eq!(
        declared(Model::Unknown, json!({}), none()),
        with(&["subagent"]),
        "unknown window = large"
    );
    assert_eq!(
        declared(Model::Small, json!({}), none()),
        with(&[]),
        "below 32K: the seven only"
    );
    assert_eq!(
        declared(Model::Flash, json!({}), none()),
        with(&[]),
        "Flash: the seven only"
    );
    // The list is sorted whatever the input order.
    let names = declared(Model::Large, json!({}), none());
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[test]
fn explicit_setting_both_ways() {
    let on = json!({"oxa": {"subagent": {"enabled": true}}});
    let off = json!({"oxa": {"subagent": {"enabled": false}}});
    for m in [Model::Large, Model::Small, Model::Flash, Model::Unknown] {
        assert_eq!(
            declared(m, on.clone(), ToolSelection::default()),
            with(&["subagent"]),
            "true: any model"
        );
        assert_eq!(
            declared(m, off.clone(), ToolSelection::default()),
            with(&[]),
            "false: never"
        );
    }
    // `true` is still subject to the allowlist rules.
    assert_eq!(declared(Model::Flash, on.clone(), tools(&["read"])), v(&["read"]));
    assert_eq!(
        declared(Model::Flash, on, tools(&["read", "subagent"])),
        v(&["read", "subagent"])
    );
    assert_eq!(
        declared(Model::Large, off, tools(&["read", "subagent"])),
        v(&["read"])
    );
}

#[test]
fn allowlist_names_only_listed_tools() {
    // `--tools`.
    assert_eq!(
        declared(Model::Large, json!({}), tools(&["read", "ls"])),
        v(&["ls", "read"])
    );
    assert_eq!(
        declared(Model::Large, json!({}), tools(&["read", "subagent"])),
        v(&["read", "subagent"])
    );
    assert_eq!(
        declared(Model::Large, json!({}), tools(&["sub*"])),
        v(&["subagent"]),
        "globs match it"
    );
    assert_eq!(
        declared(Model::Small, json!({}), tools(&["read", "subagent"])),
        v(&["read"]),
        "listed but O-146 does not offer it below 32K"
    );
    assert_eq!(
        declared(Model::Flash, json!({}), tools(&["read", "subagent"])),
        v(&["read"])
    );
    // `defaultTools` as a plain list (user or project layer): only those (extension tools stay).
    let ls = json!({"defaultTools": ["ls"]});
    assert_eq!(
        declared(Model::Large, ls, ToolSelection::default()),
        v(&["ext_tool", "ls"])
    );
    let project = Settings::from_values(json!({}), json!({"defaultTools": ["read"]}));
    let sel = ToolSelection {
        default_tools: project.default_tools(),
        default_edits: project.default_tools_edits(),
        ..Default::default()
    };
    let (set, _) = resolve_toolset(&all_tools(Model::Large, &project), &sel);
    let names: Vec<String> = set.direct.iter().map(|t| t.spec().name.clone()).collect();
    assert_eq!(names, v(&["ext_tool", "read"]), "project defaultTools");
    let with_sub = json!({"defaultTools": ["read", "subagent"]});
    assert_eq!(
        declared(Model::Large, with_sub.clone(), ToolSelection::default()),
        v(&["ext_tool", "read", "subagent"])
    );
    assert_eq!(
        declared(Model::Flash, with_sub, ToolSelection::default()),
        v(&["ext_tool", "read"])
    );
}

#[test]
fn edits_change_the_default_selection_which_includes_subagent() {
    // `--tools -ls` / `--tools +x`.
    let minus_ls: Vec<String> = with(&["subagent"]).into_iter().filter(|n| n != "ls").collect();
    assert_eq!(declared(Model::Large, json!({}), tools(&["-ls"])), minus_ls);
    let small_minus_ls: Vec<String> = with(&[]).into_iter().filter(|n| n != "ls").collect();
    assert_eq!(declared(Model::Small, json!({}), tools(&["-ls"])), small_minus_ls);
    assert_eq!(
        declared(Model::Large, json!({}), tools(&["-subagent"])),
        with(&[]),
        "-subagent removes it"
    );
    assert_eq!(
        declared(Model::Large, json!({}), tools(&["+subagent"])),
        with(&["subagent"])
    );
    assert_eq!(
        declared(Model::Flash, json!({}), tools(&["+subagent"])),
        with(&[]),
        "not offered on Flash"
    );
    // `defaultTools` made of edits only edits the default selection as well.
    let minus_bash: Vec<String> = with(&["subagent"]).into_iter().filter(|n| n != "bash").collect();
    assert_eq!(
        declared(
            Model::Large,
            json!({"defaultTools": ["-bash"]}),
            ToolSelection::default()
        ),
        minus_bash
    );
    assert_eq!(
        declared(
            Model::Large,
            json!({"defaultTools": ["-subagent"]}),
            ToolSelection::default()
        ),
        with(&[])
    );
    // A user edit `-subagent` and a project edit `+subagent`: the later layer wins.
    let s = Settings::from_values(
        json!({"defaultTools": ["-subagent"]}),
        json!({"defaultTools": ["+subagent"]}),
    );
    assert_eq!(s.default_tools_edits(), Some(v(&["-subagent", "+subagent"])));
    // Within one layer the exclusion wins, as in the settings merge.
    let s = settings(json!({"defaultTools": ["+subagent", "-subagent"]}));
    assert_eq!(s.default_tools_edits(), Some(v(&["-subagent"])));
    // A plain list in any layer is not an edit.
    let s = Settings::from_values(
        json!({"defaultTools": ["read"]}),
        json!({"defaultTools": ["+grep"]}),
    );
    assert_eq!(s.default_tools_edits(), None);
    assert_eq!(settings(json!({})).default_tools_edits(), None);
    // A settings edit followed by a CLI edit: the CLI edits the edited default selection.
    let cli_minus_sub = declared(
        Model::Large,
        json!({"defaultTools": ["-bash"]}),
        tools(&["-subagent"]),
    );
    assert_eq!(
        cli_minus_sub,
        with(&[]).into_iter().filter(|n| n != "bash").collect::<Vec<_>>()
    );
}

#[test]
fn exclude_no_builtin_and_no_tools_remove_it() {
    let xt = ToolSelection {
        exclude: v(&["subagent"]),
        ..Default::default()
    };
    assert_eq!(declared(Model::Large, json!({}), xt), with(&[]));
    let nbt = ToolSelection {
        no_builtin: true,
        ..Default::default()
    };
    assert_eq!(
        declared(Model::Large, json!({}), nbt),
        v(&["ext_tool"]),
        "-nbt: extension tools keep their rule"
    );
    let nbt_named = ToolSelection {
        no_builtin: true,
        ..tools(&["subagent"])
    };
    assert_eq!(
        declared(Model::Large, json!({}), nbt_named),
        v(&["subagent"]),
        "-nbt with --tools naming it"
    );
    let nt = ToolSelection {
        no_tools: true,
        ..Default::default()
    };
    assert!(declared(Model::Large, json!({}), nt).is_empty());
    let on = json!({"oxa": {"subagent": {"enabled": true}}});
    let nt = ToolSelection {
        no_tools: true,
        ..Default::default()
    };
    assert!(
        declared(Model::Flash, on, nt).is_empty(),
        "enabled does not survive --no-tools"
    );
}

#[test]
fn only_subagent_is_a_default_selection_member() {
    let s = settings(json!({}));
    let all = all_tools(Model::Large, &s);
    let members: Vec<String> = all
        .iter()
        .filter(|t| t.default_selected())
        .map(|t| t.spec().name.clone())
        .collect();
    assert_eq!(members, v(&["subagent"]));
}
