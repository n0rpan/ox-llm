//! Wave H, lane cache (OXA-H-TINYLOOP 3, O-146): `builtin:subagent` is on by default for every
//! model except on the Rust Flash engine and below 32K (the served model's `model_limits` cap);
//! an explicit `oxa.subagent.enabled` wins. The decision is per epoch (the tool list is part of
//! the frozen prefix, W3): `tools()` changes only with what `SessionStart` / `ModelSelect`
//! taught the hook set.

use oxa_config::{Dirs, Settings};
use oxa_core::{discover_agents, AgentDef, SubagentHooks};
use oxa_fake::TestEnv;
use oxa_proto::Hooks;
use serde_json::{json, Value};
use std::path::PathBuf;

fn hooks(settings: Value) -> SubagentHooks {
    let agent = AgentDef {
        name: "worker".into(),
        description: "A test worker".into(),
        tools: None,
        model: None,
        system_prompt: String::new(),
        path: PathBuf::from("/agents/worker.md"),
        project: false,
    };
    SubagentHooks::new(PathBuf::from("/nonexistent/oxa"), vec![agent])
        .with_settings(&Settings::from_values(settings, Value::Null))
}

#[test]
fn no_agents_means_no_declared_tool_even_when_explicitly_enabled() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    for trusted in [false, true] {
        let agents = discover_agents(&dirs, &env.work(), trusted);
        assert!(agents.is_empty(), "a fresh home has no agents");
        for settings in [
            Value::Null,
            json!({"oxa": {"subagent": {"enabled": true}}}),
            json!({"oxa": {"subagent": {"smallContext": true}}}),
        ] {
            let h = SubagentHooks::new(PathBuf::from("/nonexistent/oxa"), agents.clone())
                .with_settings(&Settings::from_values(settings, Value::Null));
            assert!(
                h.tools().is_empty(),
                "do not offer a tool that cannot start an agent"
            );
            assert!(!h.declared_for(Some(131072), false));
            assert!(!h.declared_for(None, false));
            assert!(!h.declared_for(Some(16384), true));
        }
    }
}

#[test]
fn default_is_on_except_flash_and_small_models() {
    let h = hooks(Value::Null);
    assert!(h.declared_for(Some(131072), false), "a large MoE model");
    assert!(h.declared_for(Some(32000), false), "32000 is enough");
    assert!(h.declared_for(None, false), "unknown window counts as large");
    assert!(
        !h.declared_for(Some(16384), false),
        "below 32K (model_limits cap)"
    );
    assert!(
        !h.declared_for(Some(65536), true),
        "Flash is off whatever its context"
    );
}

#[test]
fn explicit_setting_wins() {
    let on = hooks(json!({"oxa": {"subagent": {"enabled": true}}}));
    assert!(on.declared_for(Some(16384), true));
    let off = hooks(json!({"oxa": {"subagent": {"enabled": false}}}));
    assert!(!off.declared_for(Some(131072), false));
    let null = hooks(json!({"oxa": {"subagent": {"enabled": null}}}));
    assert!(null.declared_for(Some(131072), false), "null = the default rule");
    let small = hooks(json!({"oxa": {"subagent": {"smallContext": true}}}));
    assert!(
        small.declared_for(Some(16384), false),
        "the older smallContext still declares it"
    );
}
