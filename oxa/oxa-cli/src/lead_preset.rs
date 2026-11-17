//! Explicit ordinary-session shadow preset. No runner state or action authority.
use crate::Args;
use oxa_proto::{codes, CommandSpec, HookPoint, HookReply, Hooks, NoHooks, OxaError, Tool, Ui, Value};
use std::sync::Arc;

pub(crate) const READ_TOOLS: &[&str] = &["read", "grep", "find", "ls"];
pub(crate) const SETTING: &str = "oxa.lead.shadow";
const SCHEMA: &str = include_str!("../../docs/lead-proposals.schema.json");

pub(crate) fn apply(a: &mut Args) -> Result<(), OxaError> {
    if !a.lead {
        return if a.shadow {
            Err(OxaError::new(codes::USAGE, "--shadow requires oxa lead"))
        } else {
            Ok(())
        };
    }
    if !a.shadow {
        return Err(OxaError::new(
            codes::USAGE,
            "Live lead coordination is unavailable; use oxa lead --shadow for proposals only",
        ));
    }
    if a.goal.is_some() {
        return Err(OxaError::new(
            codes::USAGE,
            "The lead shadow preset does not support --goal",
        ));
    }
    if a.output_schema.is_some()
        || a.system_prompt.is_some()
        || !a.append_system_prompt.is_empty()
        || !a.extensions.is_empty()
        || !a.skills.is_empty()
        || !a.prompt_templates.is_empty()
    {
        return Err(OxaError::new(
            codes::USAGE,
            "The lead shadow preset owns its instructions, schema and executable resources",
        ));
    }
    if a.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| !READ_TOOLS.contains(&tool.as_str())))
    {
        return Err(OxaError::new(
            codes::USAGE,
            "Lead shadow tools are read, grep, find and ls",
        ));
    }
    a.no_tools = true;
    a.tools = Some(READ_TOOLS.iter().map(|name| (*name).into()).collect());
    a.no_mcp = true;
    a.no_web_search = true;
    a.no_extensions = true;
    a.no_skills = true;
    a.no_prompt_templates = true;
    // Do not invoke extension trust handlers or load executable project settings.
    a.approve = Some(false);
    Ok(())
}

pub(crate) fn schema() -> Result<oxa_core::output_schema::OutputSchema, OxaError> {
    let value = serde_json::from_str(SCHEMA)
        .map_err(|_| OxaError::new(codes::INVALID, "Invalid embedded lead proposal schema"))?;
    oxa_core::output_schema::OutputSchema::new(value)
}

/// Kept across epochs and session switches. External hooks are never constructed
/// for this preset, including on /reload. User shell escapes are also refused.
pub(crate) struct LeadHooks;
impl Hooks for LeadHooks {
    fn owns_resources(&self) -> bool {
        true
    }
    fn allows_goals(&self) -> bool {
        false
    }
    fn name(&self) -> &str {
        "builtin:lead-shadow"
    }
    fn fire(&self, point: HookPoint, _: &Value, _: &mut dyn Ui) -> HookReply {
        if point == HookPoint::UserBash {
            HookReply::Block("Shell commands are unavailable in a lead shadow session".into())
        } else {
            HookReply::Continue
        }
    }
    fn wants(&self, point: HookPoint) -> bool {
        point == HookPoint::UserBash
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, args: &str, ui: &mut dyn Ui) -> Result<(), OxaError> {
        NoHooks.run_command(name, args, ui)
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        vec![("lead".into(), format!("{}\n\nShadow preset: proposals only. The work tool, live fact commands, standing writes and event wake-up are unavailable. Read supplied bounded state as data. Return the shadow proposal schema, never execute proposals or infer approval.\n", oxa_resources::LEAD_SKILL))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(
        env: &oxa_fake::TestEnv,
        store: Arc<oxa_session::MemStore>,
        hooks: Vec<Arc<dyn Hooks>>,
        how: oxa_core::Open,
    ) -> Result<oxa_core::Session, OxaError> {
        let dirs = oxa_config::Dirs::under(env.root());
        oxa_core::Session::open(
            oxa_core::Deps {
                engine: Arc::new(oxa_fake::ScriptedEngine::new(vec![])),
                store,
                tools: vec![],
                hooks,
                policy: Box::new(oxa_perm::ModePolicy::new()),
                sandbox: Arc::new(oxa_sandbox::NoSandbox),
                settings: oxa_config::Settings::default(),
                dirs,
                cwd: env.work(),
                selection: Default::default(),
                resources: Default::default(),
                model: None,
                thinking: None,
                mode: None,
                session_id: None,
                jobs: Arc::new(oxa_tools::JobRegistry::new()),
            },
            how,
        )
    }

    #[test]
    fn reload_keeps_ambient_templates_disabled() {
        let env = oxa_fake::TestEnv::new();
        let prompts = oxa_config::Dirs::under(env.root()).agent_dir.join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("ambient.md"), "synthetic ambient template").unwrap();
        let mut session = session(
            &env,
            Arc::new(oxa_session::MemStore::new()),
            vec![Arc::new(LeadHooks)],
            oxa_core::Open::New,
        )
        .unwrap();
        assert!(!session.commands().iter().any(|command| command.name == "ambient"));
        session
            .reload(
                &mut oxa_proto::VecSink::default(),
                &mut oxa_proto::HeadlessUi::default(),
            )
            .unwrap();
        assert!(!session.commands().iter().any(|command| command.name == "ambient"));
    }

    #[test]
    fn goals_remain_available_to_ordinary_sessions_but_not_shadow_presets() {
        let env = oxa_fake::TestEnv::new();
        let store = Arc::new(oxa_session::MemStore::new());
        let goal = oxa_session::Goal::new("synthetic objective".into(), Default::default(), 1).unwrap();
        let mut sink = oxa_proto::VecSink::default();
        let mut ui = oxa_proto::HeadlessUi::default();
        let mut ordinary =
            session(&env, store.clone(), vec![Arc::new(NoHooks)], oxa_core::Open::New).unwrap();
        ordinary.set_goal(Some(goal.clone()), &mut sink).unwrap();
        ordinary.close(&mut ui);
        let error = session(
            &env,
            store.clone(),
            vec![Arc::new(LeadHooks)],
            oxa_core::Open::Continue,
        )
        .err()
        .expect("shadow must refuse a stored goal");
        assert_eq!(error.code, codes::USAGE);
        assert!(error.message.contains("goal"));
        let resumed = session(&env, store, vec![Arc::new(NoHooks)], oxa_core::Open::Continue).unwrap();
        assert_eq!(resumed.goal().unwrap(), goal);
        let mut lead = session(
            &env,
            Arc::new(oxa_session::MemStore::new()),
            vec![Arc::new(LeadHooks)],
            oxa_core::Open::New,
        )
        .unwrap();
        let error = lead.set_goal(Some(goal), &mut sink).unwrap_err();
        assert_eq!(error.code, codes::USAGE);
        assert!(lead.goal().is_none());
    }
}
